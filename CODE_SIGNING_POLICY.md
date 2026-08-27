# Code signing policy

Omendesk is an open-source remote desktop application maintained as a visible
fork of [RustDesk](https://github.com/rustdesk/rustdesk). The source code is
available at [Augn/rustdesk](https://github.com/Augn/rustdesk) under the
[GNU Affero General Public License v3](LICENCE).

Free code signing provided by [SignPath.io](https://signpath.io), certificate
by [SignPath Foundation](https://signpath.org).

## Signed releases

Official Omendesk Windows binaries are published on the
[master release page](https://github.com/Augn/rustdesk/releases/tag/master).
Once the SignPath project is approved, a release is eligible for signing only
when all of the following are true:

- The source commit belongs to this repository and is based on the `master`
  branch.
- The binaries are built by the repository's GitHub Actions workflows on
  GitHub-hosted runners.
- The unsigned workflow artifact is submitted directly from GitHub Actions to
  SignPath. Locally built or manually uploaded binaries are not eligible.
- The signing request is manually approved by an approver listed below.
- Product names and versions in signed files identify Omendesk and match the
  release being published.

Project-owned Windows executable files and libraries are signed before they
are placed in the installer. The completed portable executable and MSI are
then signed as the outer release artifacts. Third-party or upstream binaries
retain their upstream signatures and are not re-signed as Omendesk code.

## Team roles

- Authors and committers: repository maintainers with write access to
  [Augn/rustdesk](https://github.com/Augn/rustdesk).
- Reviewers: [Augn](https://github.com/Augn). Changes from contributors without
  write access require review before merge.
- Approvers: [Augn](https://github.com/Augn). Every release signing request
  requires manual approval.

Maintainers must enable multi-factor authentication for both GitHub and
SignPath accounts.

## Privacy

The project's privacy statement is available in [PRIVACY.md](PRIVACY.md).

## Reporting concerns

Report a suspected compromised or improperly signed release through a
[private GitHub security advisory](https://github.com/Augn/rustdesk/security/advisories/new).
The maintainer will investigate the affected build and request certificate or
signature revocation from SignPath when required.
