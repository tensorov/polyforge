// Standalone unit test for the C4.2 PR-comment github-script body.
// The script is extracted verbatim from action.yml and executed in a vm
// sandbox with mocked github/core/context, mirroring actions/github-script.
// Run: node .github/actions/polyforge/tests/comment-script.test.js
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const actionYml = fs.readFileSync(
  path.join(__dirname, '..', 'action.yml'),
  'utf8'
);

// Extract the github-script body: it is the final block of action.yml,
// introduced by "script: |" and indented 10 spaces.
const marker = 'script: |\n';
const idx = actionYml.lastIndexOf(marker);
assert.ok(idx !== -1, 'action.yml must contain a github-script step');
const script = actionYml
  .slice(idx + marker.length)
  .split('\n')
  .map((line) => line.replace(/^ {10}/, ''))
  .join('\n')
  .trimEnd();

// YAML-level gating invariant: every C4.2 post-step must be PR-only.
const prGuards = (actionYml.match(/if: github\.event_name == 'pull_request'/g) || []).length;
assert.equal(prGuards, 3, 'exactly 3 post-steps must be gated on pull_request');

function runScript(env, context) {
  const calls = { comments: [], failures: [] };
  const sandbox = {
    process: { env },
    core: {
      setFailed: (msg) => calls.failures.push(msg),
    },
    github: {
      rest: {
        issues: {
          createComment: async (args) => calls.comments.push(args),
        },
      },
    },
    context,
    console,
  };
  // github-script executes the body inside an async function; wrap to mirror it.
  const wrapped = `(async () => {\n${script}\n})()`;
  vm.runInNewContext(wrapped, sandbox, { filename: 'github-script-body.js' });
  return calls;
}

const env = {
  GATE_SUMMARY: 'tasks_verified=23 tasks_validated=0 tasks_failed=0',
  GATE_PASSED: 'true',
  GATE_TAIL_HASH: 'e777a340d0dbacf9a647774fbd596890b7005537ab5bfdfd2c6d554e1b435c4e',
  GATE_BUNDLE_SHA256: '619b54e1cf24579b6cc37de2c13416092057521ce0e8074473d5851bd082b1f0',
};
const context = {
  repo: { owner: 'tensorov', repo: 'polyforge' },
  issue: { number: 42 },
};

async function main() {
  // Happy path: one comment with the summary line and all three manifest facts.
  const calls = runScript(env, context);
  await calls.comments[0]; // settle the async createComment
  assert.equal(calls.comments.length, 1, 'exactly one comment posted');
  assert.equal(calls.failures.length, 0, 'no failure on happy path');
  const comment = calls.comments[0];
  assert.equal(comment.owner, 'tensorov');
  assert.equal(comment.repo, 'polyforge');
  assert.equal(comment.issue_number, 42);
  assert.match(comment.body, /tasks_verified=23 tasks_validated=0 tasks_failed=0/);
  assert.match(comment.body, /passed: true/);
  assert.match(comment.body, /tail_hash: e777a340d0dbacf9a647774fbd596890b7005537ab5bfdfd2c6d554e1b435c4e/);
  assert.match(comment.body, /bundle_sha256: 619b54e1cf24579b6cc37de2c13416092057521ce0e8074473d5851bd082b1f0/);

  // Fail-closed: any missing fact must setFailed and post nothing.
  const missing = runScript({ ...env, GATE_SUMMARY: '' }, context);
  assert.equal(missing.comments.length, 0, 'no comment when facts are missing');
  assert.equal(missing.failures.length, 1, 'setFailed called exactly once');
  assert.match(missing.failures[0], /missing ledger summary or gate manifest facts/);

  console.log('comment-script.test.js: all assertions passed');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});