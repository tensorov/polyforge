#!/usr/bin/env bash
# PolyForge T9: deterministic-enough rootfs builder for the Firecracker tier.
#
# Produces in OUT_DIR (default ./assets):
#   rootfs.ext4    64M ext4 image with the static vsock guest agent as /init
#                  plus busybox (sh and core applets) for tool execution
#   manifest.json  build-inputs manifest consumed by FcExecutor via
#                  POLYFORGE_FC_MANIFEST:
#                    {"base_image_or_packages": "...",
#                     "build_script_sha256": "<sha256 of THIS script>"}
#
# WHY INPUTS AND NOT IMAGE BYTES: mkfs.ext4 output embeds UUIDs, superblock
# timestamps, and alignment padding that differ between rebuilds even from
# identical inputs. PolyForge hashes the DECLARED INPUTS (this script's own
# sha256 plus the base package identity) so one logical image keeps one
# stable executor digest across rebuilds.
#
# Host requirements: gcc (static), e2fsprogs (mkfs.ext4 -d), curl, sha256sum.
# The guest agent source below is kept in sync with the PROVEN T8 spike
# agent (deploy/firecracker/spike/spike.sh Phase 2); single-sourcing is
# deferred to keep the spike transcript byte-stable.
set -euo pipefail

OUT_DIR="${1:-./assets}"
BUSYBOX_URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
ROOTFS_SIZE_MB="${PF_ROOTFS_SIZE_MB:-64}"

log() { printf '[rootfs-build] %s\n' "$*"; }
die() { printf '[rootfs-build] FATAL: %s\n' "$*" >&2; exit 1; }

command -v gcc >/dev/null || die "gcc not found (static guest agent build)"
command -v mkfs.ext4 >/dev/null || die "mkfs.ext4 not found (install e2fsprogs)"
command -v curl >/dev/null || die "curl not found"

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------------------
# Guest agent: static C, PID 1, AF_VSOCK listener on port 5001.
# Request line {"cmd":"..."} -> reply line {"exit":N,"stdout":"..."}.
# ---------------------------------------------------------------------------
cat > "$WORK/vsock_agent.c" <<'AGENT_EOF'
/* PolyForge T9 guest agent. Runs as PID 1 in the microVM.
 * Listens on AF_VSOCK port 5001. Request line: {"cmd":"<command>"}
 * Reply line: {"exit":N,"stdout":"..."} (stdout+stderr merged).
 * Kept in sync with deploy/firecracker/spike/spike.sh Phase 2.
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

log "building static guest agent"
gcc -static -O2 -Wall -Wextra -o "$WORK/init" "$WORK/vsock_agent.c" \
    || die "guest agent failed to compile"

log "fetching static busybox"
curl -fsSL --max-time 300 -o "$WORK/busybox" "$BUSYBOX_URL" \
    || die "busybox download failed"
chmod 755 "$WORK/busybox"

log "populating rootfs tree"
rm -rf "$WORK/rootfs-dir"
mkdir -p "$WORK/rootfs-dir"/{bin,dev,proc,sys,tmp}
cp "$WORK/init" "$WORK/rootfs-dir/init"
chmod 755 "$WORK/rootfs-dir/init"
cp "$WORK/busybox" "$WORK/rootfs-dir/bin/busybox"
ln -sf busybox "$WORK/rootfs-dir/bin/sh"

log "creating ext4 image (${ROOTFS_SIZE_MB}M) via mkfs.ext4 -d"
ROOTFS="$OUT_DIR/rootfs.ext4"
rm -f "$ROOTFS"
mkfs.ext4 -F -q -d "$WORK/rootfs-dir" "$ROOTFS" "${ROOTFS_SIZE_MB}M" \
    || die "mkfs.ext4 -d failed"

BASE_DESC="busybox 1.35.0 x86_64-linux-musl static + gcc-static vsock guest agent (spike-synced)"
SCRIPT_SHA="$(sha256sum "$0" | cut -d' ' -f1)"

cat > "$OUT_DIR/manifest.json" <<MANIFEST_EOF
{
  "base_image_or_packages": "$BASE_DESC",
  "build_script_sha256": "$SCRIPT_SHA"
}
MANIFEST_EOF

log "wrote $ROOTFS ($(stat -c%s "$ROOTFS") bytes)"
log "wrote $OUT_DIR/manifest.json (build_script_sha256=$SCRIPT_SHA)"
log "pin the kernel separately from FC CI S3; see README-host-prereqs.md"
