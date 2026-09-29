const fs = require('fs');
const path = require('path');

module.exports = async function commentOnCoverage({ github, context, core }) {
  const run = context.payload?.workflow_run;
  const contextPath = path.join(process.env.COVERAGE_ARTIFACT, 'coverage-pr-context.txt');
  const [numberText, headSha, headRepo] = fs.readFileSync(contextPath, 'utf8').trim().split('\n');
  const issue_number = Number(numberText);
  if (!Number.isSafeInteger(issue_number) || issue_number < 1) {
    throw new Error('Invalid pull request number in coverage artifact');
  }

  const { data: pull } = await github.rest.pulls.get({ ...context.repo, pull_number: issue_number });
  if (pull.state !== 'open' || pull.head.sha !== headSha ||
      pull.head.repo?.full_name !== headRepo ||
      (run && run.head_repository?.full_name !== headRepo)) {
    core.info('Ignoring coverage from a stale or mismatched pull request run');
    return;
  }

  const reportPath = path.join(process.env.COVERAGE_ARTIFACT, 'rust-coverage.xml');
  let result = 'Coverage report unavailable; check the CI run.';
  if (fs.existsSync(reportPath)) {
    const descriptor = fs.openSync(reportPath, 'r');
    const header = Buffer.alloc(8192);
    let bytesRead;
    try {
      bytesRead = fs.readSync(descriptor, header, 0, header.length, 0);
    } finally {
      fs.closeSync(descriptor);
    }
    const root = header.toString('utf8', 0, bytesRead).match(/<coverage\s+[^>]*>/)?.[0];
    const rate = root?.match(/\bline-rate="([0-9.]+)"/)?.[1];
    const covered = root?.match(/\blines-covered="(\d+)"/)?.[1];
    const valid = root?.match(/\blines-valid="(\d+)"/)?.[1];
    if (rate === undefined || covered === undefined || valid === undefined ||
        !Number.isFinite(Number(rate)) || Number(rate) < 0 || Number(rate) > 1 ||
        Number(covered) > Number(valid) || Number(valid) === 0) {
      throw new Error('Invalid Cobertura summary in coverage artifact');
    }
    result = `Line coverage: **${(Number(rate) * 100).toFixed(2)}%** ` +
      `(${Number(covered).toLocaleString('en-US')} / ${Number(valid).toLocaleString('en-US')} lines). ` +
      'Required: 75%.';
  }

  const marker = '<!-- miden-debug-coverage -->';
  const runUrl = run?.html_url || `https://github.com/${context.repo.owner}/${context.repo.repo}/actions/runs/${context.runId}`;
  const body = `${marker}\n${result}\n\n[Coverage CI run](${runUrl})`;
  const comments = await github.paginate(github.rest.issues.listComments, {
    ...context.repo, issue_number, per_page: 100,
  });
  const previous = comments.find(comment =>
    comment.user?.login === 'github-actions[bot]' && comment.body?.includes(marker));
  if (previous) {
    await github.rest.issues.updateComment({ ...context.repo, comment_id: previous.id, body });
  } else {
    await github.rest.issues.createComment({ ...context.repo, issue_number, body });
  }
};
