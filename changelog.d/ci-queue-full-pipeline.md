### Changed

- **Pull requests run a quick CI profile; the merge queue runs the full pipeline.**
  A pull request runs the three-OS unit matrix, fmt and the script tests, and
  semver. The merge queue, a manual dispatch, a workflow call, a push to main
  and any unknown event also run the release-profile suite and the oracle job
  against sgdisk, sfdisk, blkid, partx, mdadm and LVM, with every floor and
  budget unchanged. `ci-ok` remains the one required check and now fails on a
  missing or invalid plan, a missing job, and any skip the plan did not ask
  for, held to an event and result truth table in `tests/scripts`.
