# Changelog

Each release lists the project commits included since the previous release.

## 0.1.1-beta1

- feat(cursor): capture native Linux and macOS pointer shapes (`9732da4`)
- fix(files): locate native drops within the remote canvas (`eebbebd`)
- fix(cursor): preserve monochrome pointer inversion (`b6118d4`)
- fix(clipboard): decode PNG channels in fixed pixel groups (`477ef48`)
- docs(session): explain cursor, clipboard, and canvas controls (`489e008`)
- feat(cursor): stream Windows cursor deltas and render an overlay (`c6abb65`)
- feat(input): type clipboard text through remote key events (`9fb5383`)
- feat(files): upload files dropped on the remote canvas (`a73ce60`)
- fix(protocol): keep partial frames across session events (`5bca937`)
- feat(clipboard): sync native formats with configurable limits (`0dacd11`)
- feat(proto): frame and encode rich clipboard formats (`50552d5`)

## 0.1.0-beta1

- docs(readme): simplify platform setup and source builds (`9a8d7c5`)
- feat(release): record project commits in the changelog (`95c0ca6`)
- fix(ci): stage and checksum all release packages (`88520a1`)
- fix(android): configure Gradle dependency repositories (`fda00ac`)
- fix(android): restore warning-free JNI and native builds (`7a025ba`)
- fix(ci): pass the Android API level to Cargo NDK (`296ebf4`)
- fix(cli): keep host usage and desktop logs user-facing (`4814a1c`)
- fix(host): simplify the platform prompt policy check (`2f8024b`)
- feat(mcp): exercise DeskBolt channels with loopback checks (`e10c40a`)
- ci(release): verify all features and package default builds (`1977233`)
- test(release): audit desktop binaries and CLI output (`31c8a3a`)
- chore(tooling): register workspace protocol test tools (`0157468`)
- fix(ci): fix Linux dev headers and replace failing Android setup action (`87d7b3e`)
- docs(readme): update Linux build prerequisites with XCB, DRM, GBM, and fontconfig (`14b8e28`)
- fix(macos): import tracing info macro in macOS platform module (`17f545f`)
- fix(ci): fix deb/rpm pre-release versions and cargo install flags in workflow (`9905faa`)
- build(android): add gradle.properties enabling AndroidX for AGP 8.7 (`de17b62`)
- fix(scripts): add scripts directory to sys.path for unittest module invocation (`34a62de`)
- fix(ci): trigger release workflow on master and active branch (`01770dc`)
- chore(git): add conventional commit message template (`9c3e5c3`)
- docs(readme): add vector app icon, platform guides, wire protocol, and design docs (`b160dfa`)
- ci(release): add commit-driven version bumper and multi-platform release workflow (`94a6727`)
- build(packaging): add Windows NSIS installer, Linux packages, and Android build project (`994a92e`)
- feat(viewer): add desktop manager, SQLite phonebook, OOB control, and admin UI (`63ed897`)
- feat(server): add cross-platform host, Session-0 agent, policy ACLs, and admin handlers (`60ff465`)
- feat(proto): add TLS framing, Argon2id auth, codecs, and shared voice engine (`837aeef`)
- chore(repo): initialize workspace manifest, license, and gitignore rules (`ff6eae2`)
