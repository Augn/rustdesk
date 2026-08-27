# Privacy policy

Last updated: August 6, 2026

This policy describes network and data handling by the Omendesk desktop
application distributed from [Augn/rustdesk](https://github.com/Augn/rustdesk).
Omendesk is an open-source fork of RustDesk and is licensed under the GNU
Affero General Public License v3.

## Network use

This program will not transfer any information to other networked systems
unless specifically requested by the user or the person installing or
operating it. Operating the application can use the following services as
needed to provide its requested features:

- The configured rendezvous and relay service at `106.54.230.13` registers the
  device identifier, helps peers establish a connection, and relays session
  traffic when a direct connection cannot be established.
- `https://deskapi.omennew-art.com` supports optional account, device,
  address-book, authentication, and related server-backed features when those
  features are used.
- GitHub's API and release service are contacted to check for and download
  Omendesk updates from `https://github.com/Augn/rustdesk/releases/tag/master`.
- A remote session transmits the data selected by its participants, which can
  include screen images, audio, keyboard and pointer input, clipboard data,
  and files.

Network services necessarily receive standard connection information such as
source IP addresses, timestamps, device or peer identifiers, protocol data,
and request metadata. Omendesk does not ship with enabled advertising or
third-party analytics SDKs.

## Local data

Omendesk stores configuration, peer settings, logs, and other operational data
on the local device. Remote-session files are saved only to locations selected
by the participating user. Local data remains subject to the device owner's
operating-system permissions and retention choices.

## User choices

Users control whether to start a remote session, accept an incoming session,
transfer files, share clipboard content, enable audio, or use optional account
features. Closing or uninstalling Omendesk stops its normal application
network activity. Administrators can also control network access using system
or organizational policies.

## Third-party services

GitHub and any independently operated rendezvous, relay, API, proxy, or other
server selected by an administrator have their own privacy and retention
practices. This repository does not control independently operated services.

## Contact

Privacy questions and data requests can be submitted through the
[repository issue tracker](https://github.com/Augn/rustdesk/issues). Security
reports that should not be public can be submitted through a
[private GitHub security advisory](https://github.com/Augn/rustdesk/security/advisories/new).
