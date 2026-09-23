# Code signing policy

Omendesk is an open-source remote desktop application maintained as a visible
fork of [RustDesk](https://github.com/rustdesk/rustdesk). The source code is
available at [Augn/rustdesk](https://github.com/Augn/rustdesk) under the
[GNU Affero General Public License v3](LICENCE).

Windows release signing uses the organization's hardware-backed EV code-signing
certificate issued by Certum. The private key remains on the smart card.

## Signed releases

Official Omendesk Windows binaries are published on the
[GitHub Releases page](https://github.com/Augn/rustdesk/releases).
A Windows release is eligible for signing only when all of the following are
true:

- The source commit belongs to this repository and is based on the master
  branch.
- The application binaries are built by this repository's GitHub Actions
  workflows on GitHub-hosted runners and downloaded from that run.
- The maintainer signs project-owned Windows executables and libraries locally
  with the Certum smart card, then packages the signed files into the portable
  executable and MSI. The same certificate signs both outer packages.
- SHA-256 Authenticode signatures have RFC 3161 timestamps. The maintainer
  verifies the signer, timestamp, package signatures, and release hashes
  before publication.
- Product names and versions in signed files identify Omendesk and match the
  release being published.

Third-party or upstream binaries retain their upstream signatures and are not
re-signed as Omendesk code.

## Team roles

- Authors and committers: repository maintainers with write access to
  [Augn/rustdesk](https://github.com/Augn/rustdesk).
- Reviewers: [Augn](https://github.com/Augn). Changes from contributors without
  write access require review before merge.
- Release signer: [Augn](https://github.com/Augn). The smart-card PIN is entered
  only on the signing workstation and never stored in GitHub secrets.

Maintainers must enable multi-factor authentication for GitHub accounts.

## Privacy

The project's privacy statement is available in [PRIVACY.md](PRIVACY.md).

## Reporting concerns

Report a suspected compromised or improperly signed release through a
[private GitHub security advisory](https://github.com/Augn/rustdesk/security/advisories/new).
The maintainer will investigate the affected build and request certificate or
signature revocation from Certum when required.
