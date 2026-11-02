# Agent instructions

## Host ABI

The extension targets `attricat:host@1.1.0` (`host_api` `^1.1`) with the
`outcome-extension` world. `crates/component/wit/attricat-extension.wit` is the
[attricat-extension-sdk](https://github.com/attricat/attricat-extension-sdk)
`wit/attricat-host-1.1.0.wit` verbatim (`just check-wit`). Never copy the core
repository's unreleased `wit-host/`, and never edit the vendored file by hand.

## Limits worth remembering

A call has 10 seconds and 1 billion fuel; going over quarantines the extension
for the whole workspace. An input artifact can't seek, so a batch re-reads the
file up to its position, under the read budget in `import.rs`. A batch's rows
must not depend on time: a replayed batch has to append the same report bytes.

## Verification

Unit tests are necessary but don't prove host integration. Run `just pack`,
then `just e2e` against a running development server, and extend `e2e/verify.mjs`
when you change a contribution or operation. It installs and removes this
extension, and leaves its test blueprints behind.

`just host-e2e` runs core's packaged tests. They grant only `attricat.read`,
`attricat.write`, `artifacts.read` and `artifacts.write`, and expect a 17-row
import to take two batches, so they need updating in core with this release.
