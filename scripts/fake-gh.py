#!/usr/bin/env python3
"""A stand-in `gh` for native verification of the Pull request surface. It never reaches GitHub.

`python3 scripts/fake-gh.py install DIR REPO` makes DIR/bin/gh, a link to this script, and starts at
the `running` stage. Only REPO's branch has the pull request; gh run in any other folder finds none. Put DIR/bin first on the app's PATH. Each call appends its arguments as one JSON
line to DIR/calls.log and answers from the stage named in DIR/stage:

  none       the branch has no pull request
  running    pull request #12 is open with one check running and one passed
  failing    the running check failed
  review     a review requests changes, with a comment on src/lib.rs:2
  mergeable  every check passed and the review approved; GitHub can merge
  blocked    as mergeable, but the base branch's rules block the merge
  merged     the pull request is merged

Write a stage name to DIR/stage to move on. The failed test check ran as Actions job 9311, whose
failed steps' log (`gh run view --job 9311 --log-failed`) is about 76 KiB: FIRST-LINE and LAST-LINE
end it, and MIDDLE-LINE sits where a 32 KiB head and tail leave a gap. The repository allows squash
and merge commits. A merge
(`gh api --method PUT .../merge`) is recorded and moves the stage to `merged`; `exit4` as the stage
makes every call fail as a missing login does.
"""
import json
import os
import sys
from pathlib import Path

URL = 'https://github.com/keystone-fixture/demo/pull/12'
TIME = '2026-10-25T08:00:00Z'
UNSET = '0001-01-01T00:00:00Z'


JOB = 'https://github.com/keystone-fixture/demo/actions/runs/4207/job/9311'


def check(name, status, conclusion='', completed=UNSET, started=TIME, url=''):
    return {'__typename': 'CheckRun', 'name': name, 'workflowName': 'CI', 'status': status,
            'conclusion': conclusion, 'startedAt': started, 'completedAt': completed,
            'detailsUrl': url}


def failed_log():
    steps = ''.join(f'test\tRun cargo test\t2026-10-25T08:02:{n % 60:02d}.0000000Z test upload::case_{n} ... ok\n'
                    for n in range(500))
    return ('test\tRun cargo test\t2026-10-25T08:01:00.0000000Z FIRST-LINE running 1001 tests\n' + steps
            + 'test\tRun cargo test\t2026-10-25T08:02:30.0000000Z MIDDLE-LINE\n' + steps
            + 'test\tRun cargo test\t2026-10-25T08:03:11.0000000Z thread \'upload::retries_three_times\' panicked at src/lib.rs:2:\n'
            + 'test\tRun cargo test\t2026-10-25T08:03:11.0000000Z assertion failed: tries <= 3\n'
            + 'test\tRun cargo test\t2026-10-25T08:03:12.0000000Z error: test failed, LAST-LINE\n')


LINT = check('lint', 'COMPLETED', 'SUCCESS', '2026-10-25T08:00:41Z')
CHECKS = {
    'running': [check('test', 'IN_PROGRESS'), LINT],
    'failing': [check('test', 'COMPLETED', 'FAILURE', '2026-10-25T08:03:12Z', url=JOB), LINT],
    'review': [check('test', 'COMPLETED', 'FAILURE', '2026-10-25T08:03:12Z', url=JOB), LINT],
    'mergeable': [check('test', 'COMPLETED', 'SUCCESS', '2026-10-25T08:12:30Z',
                        '2026-10-25T08:10:00Z'), LINT],
}
CHECKS['merged'] = CHECKS['blocked'] = CHECKS['mergeable']
REVIEW = {'id': 'PRR_1', 'author': {'login': 'ana'}, 'state': 'CHANGES_REQUESTED',
          'body': 'The retry needs a limit.', 'submittedAt': '2026-10-25T08:05:00Z'}
APPROVAL = {'id': 'PRR_2', 'author': {'login': 'ana'}, 'state': 'APPROVED', 'body': '',
            'submittedAt': '2026-10-25T08:13:00Z'}
COMMENT = {'id': 7, 'user': {'login': 'ana'}, 'path': 'src/lib.rs', 'line': 2,
           'body': 'Cap this at three tries.'}


def view(stage):
    decision = {'review': 'CHANGES_REQUESTED', 'mergeable': 'APPROVED', 'blocked': 'APPROVED',
                'merged': 'APPROVED'}.get(stage, 'REVIEW_REQUIRED')
    reviews = {'review': [REVIEW], 'mergeable': [REVIEW, APPROVAL], 'blocked': [REVIEW, APPROVAL],
               'merged': [REVIEW, APPROVAL]}.get(stage, [])
    return {'title': 'Retry flaky uploads',
            'state': 'MERGED' if stage == 'merged' else 'OPEN',
            'isDraft': False, 'baseRefName': 'main', 'headRefName': 'retry-uploads',
            'headRefOid': '3f2c9e1', 'reviewDecision': decision,
            'mergeStateStatus': {'mergeable': 'CLEAN', 'merged': 'UNKNOWN'}.get(stage, 'BLOCKED'),
            'statusCheckRollup': CHECKS[stage], 'reviews': reviews}


def answer(home, args):
    stage = (home / 'stage').read_text().strip()
    if stage == 'exit4':
        print('To get started with GitHub CLI, please run:  gh auth login', file=sys.stderr)
        return 4
    if args[:2] == ['pr', 'view'] and args[2:3] == ['--json']:
        if stage == 'none' or Path.cwd() != Path((home / 'repo').read_text().strip()):
            print('no pull requests found for branch "retry-uploads"', file=sys.stderr)
            return 1
        print(json.dumps({'number': 12, 'url': URL, 'state': 'OPEN'}))
    elif args[:2] == ['pr', 'view']:
        print(json.dumps(view(stage)))
    elif args[:2] == ['run', 'view'] and '--log-failed' in args:
        sys.stdout.write(failed_log())
    elif args[:2] == ['repo', 'view']:
        print(json.dumps({'squashMergeAllowed': True, 'mergeCommitAllowed': True,
                          'rebaseMergeAllowed': False}))
    elif args[:1] == ['api'] and 'user' in args:
        print('octocat')
    elif args[:1] == ['api'] and 'PUT' in args:
        (home / 'stage').write_text('merged\n')
        print(json.dumps({'merged': True}))
    elif args[:1] == ['api']:
        print(json.dumps([COMMENT] if stage in ('review', 'mergeable', 'blocked', 'merged') else []))
    else:
        print(f'fake gh does not know: {" ".join(args)}', file=sys.stderr)
        return 1
    return 0


def main():
    if sys.argv[1:2] == ['install']:
        home = Path(sys.argv[2]).absolute()
        (home / 'bin').mkdir(parents=True, exist_ok=True)
        link = home / 'bin' / 'gh'
        if not link.exists():
            os.symlink(Path(__file__).resolve(), link)
        (home / 'stage').write_text('running\n')
        (home / 'repo').write_text(str(Path(sys.argv[3]).resolve()) + '\n')
        print(home / 'bin')
        return 0
    home = Path(sys.argv[0]).absolute().parent.parent
    with open(home / 'calls.log', 'a') as log:
        log.write(json.dumps(sys.argv[1:]) + '\n')
    return answer(home, sys.argv[1:])


if __name__ == '__main__':
    sys.exit(main())
