#!/usr/bin/env bash
# PolyForge T8 spike: Firecracker microVM vsock exec protocol go/no-go.
#
# Proves on a KVM host that:
#   1. A Firecracker microVM boots from a minimal ext4 rootfs.
#   2. The host completes the vsock CONNECT handshake through the FC unix socket.
#   3. A command sent as newline-delimited JSON executes in the guest and
#      stdout plus exit code come back as JSON.
#
# Self-contained: embeds the guest agent C source and the host-side python3
# vsock client. All binaries and images live under /tmp/fc-spike (temp only).
# Nothing here is production code; this is the T8 go/no-go data point.
#
# Usage: ./spike.sh [--keep]     (--keep leaves the VM running for inspection)
# Exit:  0 = GO, 1 = NO-GO.
set -euo pipefail

WORK="${FC_SPIKE_WORK:-/tmp/fc-spike}"
BIN="$WORK/bin"
ASSETS="$WORK/assets"
ROOTFS_DIR="$WORK/rootfs-dir"
API_SOCK="$WORK/api.sock"
VSOCK_SOCK="$WORK/v.sock"
FC_LOG="$WORK/fc.log"
FC_VERSION="1.9.1"
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.102"
BUSYBOX_URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
FC_TGZ_URL="https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-x86_64.tgz"
BOOT_TIMEOUT="${FC_SPIKE_BOOT_TIMEOUT:-90}"

log() { printf '[spike] %s\n' "$*"; }
die() { printf '[spike] FATAL: %s\n' "$*" >&2; exit 1; }

FC_PID=""
cleanup() {
    if [[ -n "$FC_PID" ]] && kill -0 "$FC_PID" 2>/dev/null; then
        kill "$FC_PID" 2>/dev/null || true
        wait "$FC_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Phase 1: assets (idempotent; everything cached under $WORK)
# ---------------------------------------------------------------------------
prepare_assets() {
    mkdir -p "$BIN" "$ASSETS"

    if [[ ! -x "$BIN/firecracker" ]]; then
        log "downloading Firecracker v${FC_VERSION} + jailer"
        curl -sSL --max-time 300 "$FC_TGZ_URL" | tar xz -C "$BIN"
        cp "$BIN/release-v${FC_VERSION}-x86_64/firecracker-v${FC_VERSION}-x86_64" "$BIN/firecracker"
        cp "$BIN/release-v${FC_VERSION}-x86_64/jailer-v${FC_VERSION}-x86_64" "$BIN/jailer"
        chmod +x "$BIN/firecracker" "$BIN/jailer"
    fi
    log "firecracker: $("$BIN/firecracker" --version)"

    if [[ ! -s "$ASSETS/vmlinux" ]]; then
        log "downloading kernel vmlinux-6.1.102 from FC CI S3 bucket"
        curl -sSL --max-time 600 -o "$ASSETS/vmlinux" "$KERNEL_URL"
    fi
    file "$ASSETS/vmlinux" | grep -q 'ELF' || die "kernel download is not an ELF image (S3 key may have moved)"
    log "kernel: $(stat -c%s "$ASSETS/vmlinux") bytes"

    if [[ ! -x "$ASSETS/busybox" ]]; then
        log "downloading static busybox"
        curl -sSL --max-time 300 -o "$ASSETS/busybox" "$BUSYBOX_URL"
        chmod +x "$ASSETS/busybox"
    fi
}

# ---------------------------------------------------------------------------
# Phase 2: rootfs with the embedded static vsock agent as /init
# ---------------------------------------------------------------------------
build_rootfs() {
    local agent_c="$WORK/vsock_agent.c"
    cat > "$agent_c" <<'AGENT_EOF'
/* T8 spike guest agent. Runs as PID 1 in the microVM.
 * Listens on AF_VSOCK port 5001. Request line: {"cmd":"<command>"}
 * Reply line: {"exit":N,"stdout":"..."} (stdout+stderr merged, JSON-escaped).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/vm_sockets.h>

#define PORT 5001
#define RBUF_SZ 65536

static void json_escape(const char *in, size_t n, char *out, size_t out_sz) {
    size_t o = 0;
    for (size_t i = 0; i < n && o + 7 < out_sz; i++) {
        unsigned char c = (unsigned char)in[i];
        switch (c) {
        case '"':  out[o++]='\\'; out[o++]='"';  break;
        case '\\': out[o++]='\\'; out[o++]='\\'; break;
        case '\n': out[o++]='\\'; out[o++]='n';  break;
        case '\r': out[o++]='\\'; out[o++]='r';  break;
        case '\t': out[o++]='\\'; out[o++]='t';  break;
        default:
            if (c < 0x20) { snprintf(out + o, out_sz - o, "\\u%04x", c); o += 6; }
            else out[o++] = (char)c;
        }
    }
    out[o] = 0;
}

static int extract_cmd(const char *buf, char *out, size_t out_sz) {
    const char *k = strstr(buf, "\"cmd\"");
    if (!k) return -1;
    const char *p = strchr(k + 5, ':');
    if (!p) return -1;
    p++;
    while (*p == ' ' || *p == '\t') p++;
    if (*p != '"') return -1;
    p++;
    size_t o = 0;
    while (*p && *p != '"' && o + 1 < out_sz) {
        if (*p == '\\' && p[1]) {
            p++;
            switch (*p) {
            case 'n': out[o++]='\n'; break;
            case 't': out[o++]='\t'; break;
            case 'r': out[o++]='\r'; break;
            default:  out[o++]=*p;   break;
            }
        } else {
            out[o++] = *p;
        }
        p++;
    }
    if (*p != '"') return -1;
    out[o] = 0;
    return 0;
}

static void handle_conn(int fd) {
    static char rbuf[RBUF_SZ], cmd[RBUF_SZ], obuf[RBUF_SZ], esc[RBUF_SZ * 2];
    for (;;) {
        size_t got = 0;
        while (got < sizeof(rbuf) - 1) {
            ssize_t n = read(fd, rbuf + got, sizeof(rbuf) - 1 - got);
            if (n <= 0) {
                if (got == 0) return;
                break;
            }
            got += (size_t)n;
            rbuf[got] = 0;
            if (memchr(rbuf, '\n', got)) break;
        }
        rbuf[got] = 0;
        if (extract_cmd(rbuf, cmd, sizeof(cmd)) != 0) {
            dprintf(fd, "{\"error\":\"bad request\"}\n");
            continue;
        }
        int pfd[2];
        if (pipe(pfd) != 0) { dprintf(fd, "{\"error\":\"pipe\"}\n"); continue; }
        pid_t pid = fork();
        if (pid < 0) { dprintf(fd, "{\"error\":\"fork\"}\n"); continue; }
        if (pid == 0) {
            close(pfd[0]);
            dup2(pfd[1], 1);
            dup2(pfd[1], 2);
            close(pfd[1]);
            execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
            _exit(127);
        }
        close(pfd[1]);
        size_t total = 0;
        while (total < sizeof(obuf) - 1) {
            ssize_t n = read(pfd[0], obuf + total, sizeof(obuf) - 1 - total);
            if (n <= 0) break;
            total += (size_t)n;
        }
        close(pfd[0]);
        int status = 0;
        waitpid(pid, &status, 0);
        int code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
        json_escape(obuf, total, esc, sizeof(esc));
        dprintf(fd, "{\"exit\":%d,\"stdout\":\"%s\"}\n", code, esc);
    }
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    int s = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (s < 0) {
        fprintf(stderr, "agent: socket(AF_VSOCK): %s\n", strerror(errno));
        sleep(3);
        return 1;
    }
    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_port = PORT;
    addr.svm_cid = VMADDR_CID_ANY;
    if (bind(s, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        fprintf(stderr, "agent: bind vsock:%d: %s\n", PORT, strerror(errno));
        return 1;
    }
    if (listen(s, 8) != 0) {
        fprintf(stderr, "agent: listen: %s\n", strerror(errno));
        return 1;
    }
    fprintf(stderr, "agent: listening on vsock:%d\n", PORT);
    for (;;) {
        int fd = accept(s, NULL, NULL);
        if (fd < 0) continue;
        pid_t w = fork();
        if (w == 0) { close(s); handle_conn(fd); _exit(0); }
        close(fd);
        while (waitpid(-1, NULL, WNOHANG) > 0) {}
    }
}
AGENT_EOF

    rm -rf "$ROOTFS_DIR"
    mkdir -p "$ROOTFS_DIR"/{bin,dev,proc,sys,tmp}
    gcc -static -O2 -Wall -Wextra -o "$ROOTFS_DIR/init" "$agent_c" \
        || die "guest agent failed to compile"
    cp "$ASSETS/busybox" "$ROOTFS_DIR/bin/busybox"
    chmod 755 "$ROOTFS_DIR/bin/busybox"
    ln -sf busybox "$ROOTFS_DIR/bin/sh"

    rm -f "$ASSETS/rootfs.ext4"
    mkfs.ext4 -F -q -d "$ROOTFS_DIR" "$ASSETS/rootfs.ext4" 64M \
        || die "mkfs.ext4 -d failed"
    log "rootfs built: $(stat -c%s "$ASSETS/rootfs.ext4") bytes ($(ls "$ROOTFS_DIR" | tr '\n' ' '))"
}

# ---------------------------------------------------------------------------
# Phase 3: FC API helpers over the unix socket
# ---------------------------------------------------------------------------
api_put() { # api_put <path> <json-body>; echoes http code; body to stderr
    local resp code body
    resp=$(curl -sS --max-time 10 --unix-socket "$API_SOCK" \
        -X PUT "http://localhost/$1" \
        -H 'Content-Type: application/json' \
        -d "$2" -w '\n%{http_code}') || die "curl to FC API failed on PUT /$1"
    code=$(printf '%s' "$resp" | tail -n1)
    body=$(printf '%s' "$resp" | sed '$d')
    [[ -n "$body" ]] && printf '[fc-api] PUT /%s -> %s %s\n' "$1" "$code" "$body" >&2 \
        || printf '[fc-api] PUT /%s -> %s\n' "$1" "$code" >&2
    [[ "$code" =~ ^2[0-9][0-9]$ ]] || die "PUT /$1 rejected ($code)"
}

start_vm() {
    rm -f "$API_SOCK" "$VSOCK_SOCK" "$FC_LOG"
    # FC v1.9.1 opens the logger target WITHOUT O_CREAT: the file must exist.
    : > "$WORK/fc-internal.log"
    "$BIN/firecracker" --api-sock "$API_SOCK" >"$FC_LOG" 2>&1 &
    FC_PID=$!
    for _ in $(seq 1 50); do
        [[ -S "$API_SOCK" ]] && break
        kill -0 "$FC_PID" 2>/dev/null || die "firecracker exited early; see $FC_LOG"
        sleep 0.2
    done
    [[ -S "$API_SOCK" ]] || die "FC API socket never appeared"

    local code
    api_put logger "{\"level\":\"Info\",\"log_path\":\"$WORK/fc-internal.log\",\"show_level\":true,\"show_log_origin\":true}"
    api_put boot-source "{\"kernel_image_path\":\"$ASSETS/vmlinux\",\"boot_args\":\"console=ttyS0 init=/init reboot=k panic=1 pci=off\"}"
    api_put drives/rootfs "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$ASSETS/rootfs.ext4\",\"is_root_device\":true,\"is_read_only\":false,\"io_engine\":\"Sync\"}"
    api_put vsock "{\"guest_cid\":3,\"uds_path\":\"$VSOCK_SOCK\"}"
    api_put actions '{"action_type":"InstanceStart"}'
    log "microVM started (pid $FC_PID, cid 3)"
}

# ---------------------------------------------------------------------------
# Phase 4: host-side vsock client (CONNECT handshake + exec round-trip)
# ---------------------------------------------------------------------------
vsock_roundtrip() {
python3 - "$VSOCK_SOCK" "$BOOT_TIMEOUT" <<'PY_EOF'
import json
import socket
import sys
import time

uds_path = sys.argv[1]
deadline = time.time() + float(sys.argv[2])
REQUEST = b'{"cmd":"echo hello-from-firecracker"}\n'


def attempt():
    """One full protocol cycle. Returns (ok, detail)."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(15)
    try:
        s.connect(uds_path)
        # CONNECT handshake per docs/vsock.md: send CONNECT <port>, expect OK <port>.
        s.sendall(b"CONNECT 5001\n")
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(4096)
            if not chunk:
                return False, "uds closed during CONNECT handshake"
            buf += chunk
        line, _ = buf.split(b"\n", 1)
        if not line.startswith(b"OK"):
            return False, f"CONNECT rejected: {line!r}"
        # Raw bidirectional stream from here.
        s.sendall(REQUEST)
        resp = b""
        while b"\n" not in resp:
            chunk = s.recv(65536)
            if not chunk:
                return False, "guest closed stream before response"
            resp += chunk
        line, _ = resp.split(b"\n", 1)
        obj = json.loads(line.decode())
        print(f"[vsock] guest response: {json.dumps(obj)}")
        if obj.get("exit") == 0 and "hello-from-firecracker" in str(obj.get("stdout", "")):
            return True, obj
        return False, f"unexpected response: {obj}"
    except OSError as e:
        return False, f"{type(e).__name__}: {e}"
    finally:
        s.close()


last = None
while True:
    ok, detail = attempt()
    if ok:
        print("[vsock] echo round-trip OK (exit=0, stdout contains marker)")
        sys.exit(0)
    last = detail
    if time.time() > deadline:
        print(f"[vsock] giving up: {last}", file=sys.stderr)
        sys.exit(1)
    time.sleep(0.5)
PY_EOF
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
[[ -e /dev/kvm ]] || die "/dev/kvm not present on this host"
log "workdir: $WORK"

prepare_assets
build_rootfs
start_vm

log "waiting for vsock round-trip (timeout ${BOOT_TIMEOUT}s)"
if vsock_roundtrip; then
    log "VERDICT: GO - Firecracker microVM boot + vsock exec protocol proven on this host"
    exit 0
else
    log "VERDICT: NO-GO - protocol did not complete within timeout"
    log "--- last 40 lines of serial console ($FC_LOG) ---"
    tail -n 40 "$FC_LOG" >&2 || true
    exit 1
fi
