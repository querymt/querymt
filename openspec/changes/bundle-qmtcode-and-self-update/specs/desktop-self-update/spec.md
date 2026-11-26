# Spec Delta

## Purpose

Defines how QueryMT Desktop learns about new releases from a signed release manifest and how it downloads, verifies, installs and relaunches into them, with the user in control of when that happens.

## ADDED Requirements

### Requirement: Releases publish a signed manifest with desktop update assets
Each stable and nightly release SHALL publish `release.json` with a `desktop_updates` object, which maps each self-updatable platform to the asset's file name, size in bytes and SHA-256. Releases SHALL also publish `release.json.minisig`, a minisign signature of the exact `release.json` bytes, whose trusted comment carries `version:<version>` and `channel:<channel>`.

#### Scenario: Manifest lists every self-updatable platform
- **WHEN** a release is published
- **THEN** `release.json` has `desktop_updates` entries for `darwin-aarch64`, `darwin-x86_64`, `windows-x86_64`, `linux-x86_64` and `linux-aarch64`, and each entry's size and SHA-256 match the published asset it names

#### Scenario: Manifest keeps the existing fields
- **WHEN** a release is published
- **THEN** `release.json` still has `version`, `channel`, `commit` and `published_at` with their existing meaning

#### Scenario: Signature covers the manifest
- **WHEN** `release.json.minisig` is checked against `release.json` with the committed update public key
- **THEN** the signature verifies, and its trusted comment names the manifest's version and channel

#### Scenario: Missing asset or key fails the release
- **WHEN** a release run lacks a desktop asset named in the platform list, or the signing key is not available
- **THEN** the run fails before publishing anything

### Requirement: Only release builds check for updates
A build published by the release pipeline SHALL know its channel: stable, or nightly when its version has a `nightly` pre-release. Other builds SHALL make no update requests and SHALL show updates as unavailable for this build. This covers local, development and Nix builds. The web build SHALL have no updater.

#### Scenario: Development build makes no update request
- **WHEN** a build made without a release version runs for longer than the first-check delay
- **THEN** it has made no update request, and Settings says updates are not available for this build

#### Scenario: Nightly build follows the nightly channel
- **WHEN** a build with version `0.6.1-nightly.20261126120000` starts with no saved channel
- **THEN** its update channel is nightly

### Requirement: Update checks follow the schedule and preferences
With automatic checks on, the app SHALL check 30 seconds after start and then every 6 hours while running. The user SHALL be able to start a check at any time. Only one check SHALL run at a time. A check SHALL request only the manifest and signature of the selected channel.

#### Scenario: First automatic check after start
- **WHEN** a release build starts with automatic checks on
- **THEN** it checks for updates about 30 seconds later, and again every 6 hours while running

#### Scenario: Automatic checks off
- **WHEN** automatic checks are off
- **THEN** the app makes no update request until the user selects "Check now"

#### Scenario: Concurrent check is ignored
- **WHEN** the user selects "Check now" while a check is running
- **THEN** no second check starts and the running check's result is shown

### Requirement: Only verified newer versions are offered
The app SHALL offer an update only when all of these hold:
- the signature verifies with the update public key built into the app;
- the trusted comment's version and channel equal the manifest's;
- the manifest's channel is the selected channel;
- the version is strictly greater than the running version under semver precedence.

#### Scenario: Newer signed version is offered
- **WHEN** a check finds a correctly signed manifest on the selected channel with a version above the running one
- **THEN** the app shows that version as available

#### Scenario: Same or older version is not offered
- **WHEN** a check finds a correctly signed manifest whose version is equal to or below the running one
- **THEN** the app reports that it is up to date and offers nothing

#### Scenario: Bad signature is refused
- **WHEN** the manifest's signature does not verify with the built-in key
- **THEN** nothing is offered, and a manual check shows that the release could not be verified

#### Scenario: Signature for another version is refused
- **WHEN** the signature verifies but its trusted comment names a different version or channel than the manifest
- **THEN** nothing is offered

#### Scenario: Switching to stable never downgrades
- **WHEN** a nightly build switches to the stable channel and the latest stable is older than the running nightly
- **THEN** nothing is offered until a stable newer than the running version is published

### Requirement: Skipped versions are not announced
The user SHALL be able to skip an offered version. Automatic checks SHALL NOT announce a skipped version. A manual check SHALL still show it, and a newer version SHALL be announced normally.

#### Scenario: Skipped version stays quiet
- **WHEN** the user skips version X and a later automatic check finds X
- **THEN** no update is announced

#### Scenario: Manual check shows skipped version
- **WHEN** the user skips version X and then selects "Check now" while X is the latest
- **THEN** X is shown as available

### Requirement: Downloads happen only on request and are verified
The app SHALL download an update only after the user asks for it, and SHALL show progress. Before using the download, the app SHALL check its size and SHA-256 against the verified manifest entry for its platform. On any mismatch it SHALL discard the download, report the failure and change nothing.

#### Scenario: No download without consent
- **WHEN** an update is available and the user has not chosen to install it
- **THEN** no update asset has been downloaded

#### Scenario: Corrupted download is rejected
- **WHEN** a downloaded asset's SHA-256 differs from the manifest entry
- **THEN** the download is deleted, an error is shown, and the installed app is unchanged

#### Scenario: Nightly replaced during download
- **WHEN** the nightly release is replaced between the check and the download, so the asset no longer matches
- **THEN** the install fails with the same verification error and the next check offers the new nightly

### Requirement: Install method depends on how the app was installed
The app SHALL install updates itself only when it runs from one of these:
- a macOS `.app` in a writable location that is not App Translocation;
- a Windows NSIS install;
- an AppImage whose directory is writable.

For any other install, including deb, Nix, read-only or translocated, it SHALL show the new version with a link to its release page and SHALL NOT download it.

#### Scenario: Deb install is notify-only
- **WHEN** a newer version is found by an app installed from the deb
- **THEN** the app shows the version and a link to the release page, and offers no install action

#### Scenario: Translocated macOS app is notify-only
- **WHEN** a newer version is found by a macOS app running from an App Translocation path
- **THEN** the app shows the version and tells the user to move the app to Applications to get automatic updates

### Requirement: Installing replaces the app safely and relaunches
To install, the app SHALL stop its local process agents first. If installation fails, the previous version SHALL stay installed and runnable. After a successful install, the app SHALL offer to restart now or later; restarting SHALL start the new version as a single instance.

#### Scenario: Local agents stop before files change
- **WHEN** the user installs an update while a local qmtcode agent is running
- **THEN** the agent process has exited before any installed file is replaced

#### Scenario: macOS bundle is replaced in place
- **WHEN** a verified update is installed on macOS
- **THEN** `QueryMT Desktop.app` at its current location contains the new version and the new bundled qmtcode

#### Scenario: AppImage is replaced in place
- **WHEN** a verified update is installed from an AppImage
- **THEN** the AppImage file at its original path is the new version and is executable

#### Scenario: Windows installer runs after the app exits
- **WHEN** a verified update is installed on Windows
- **THEN** the app quits, the setup runs without prompts once the app's files are free, and the new version starts afterwards

#### Scenario: Failed install keeps the old version
- **WHEN** replacing the installed files fails partway
- **THEN** the previously installed version is in place and starts normally, and an error is shown

#### Scenario: Restart opens the new version
- **WHEN** the user selects "Restart now" after an install on macOS or AppImage
- **THEN** the running app quits and exactly one instance of the new version starts

### Requirement: Update preferences persist
Settings SHALL show the running version and the update channel, with stable or nightly selectable and the build's channel as the default. Settings SHALL also have the automatic-check switch (on by default), a "Check now" action and the last check time. Channel, automatic checks and skipped version SHALL persist across restarts, and state files without them SHALL load with the defaults.

#### Scenario: Preferences survive restart
- **WHEN** the user selects the nightly channel, turns automatic checks off and restarts the app
- **THEN** the channel is nightly and automatic checks are off

#### Scenario: Older state loads with update defaults
- **WHEN** the app loads a saved state file written before update preferences existed
- **THEN** it loads without error with the build's channel and automatic checks on

### Requirement: Failed checks are quiet unless requested
A failed automatic check, such as a network error or a missing manifest, SHALL be logged and SHALL NOT interrupt the user. A failed manual check SHALL show the reason.

#### Scenario: Automatic check offline
- **WHEN** an automatic check cannot reach the release server
- **THEN** no error is shown to the user, and the failure is logged

#### Scenario: Manual check offline
- **WHEN** a manual check cannot reach the release server
- **THEN** Settings shows that the check failed and why
