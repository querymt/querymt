# Spec Delta

## Purpose

Defines what the QueryMT Desktop installers published with each QueryMT release contain, where the bundled qmtcode lives, and the identity the installed app carries.

## ADDED Requirements

### Requirement: Installers bundle the qmtcode of the same release
Every QueryMT Desktop installer published by a release SHALL contain the qmtcode binary built by that same release run for the installer's platform and architecture. Linux installers SHALL contain the statically linked (musl) qmtcode.

#### Scenario: macOS app bundle contains qmtcode
- **WHEN** a release publishes the macOS `.dmg` and `.app.tar.gz` for an architecture
- **THEN** the app bundle in both contains an executable `Contents/MacOS/qmtcode` for that architecture, and the bundle passes a strict code signature check

#### Scenario: Windows installer installs qmtcode next to the app
- **WHEN** the Windows setup exe is run
- **THEN** `qmtcode.exe` is installed in the same directory as the app executable

#### Scenario: Windows uninstaller removes qmtcode
- **WHEN** QueryMT Desktop is uninstalled on Windows
- **THEN** `qmtcode.exe` is removed together with the app executable

#### Scenario: Linux packages install qmtcode outside PATH
- **WHEN** the deb is installed or the AppImage is mounted
- **THEN** qmtcode is at `usr/lib/querymt-gpui/qmtcode` below the install root, and the deb installs nothing named `qmtcode` in `/usr/bin`

#### Scenario: Bundled qmtcode matches the release version
- **WHEN** the bundled qmtcode of any published installer is run with `--version`
- **THEN** it reports the same version as the release that published the installer

### Requirement: Release fails when an installer lacks qmtcode
The release build SHALL check each desktop installer it produces for the bundled qmtcode and SHALL fail without publishing when any installer lacks it. Packaging in CI SHALL fail when no qmtcode binary is supplied. Local packaging without one SHALL warn and continue.

#### Scenario: CI packaging without qmtcode fails
- **WHEN** a packaging script runs in CI without a qmtcode binary
- **THEN** it exits with an error naming the missing input and writes no installer

#### Scenario: Local packaging without qmtcode warns
- **WHEN** a packaging script runs outside CI without a qmtcode binary
- **THEN** it prints a warning that the installer has no bundled qmtcode and still writes the installer

#### Scenario: Installer content check fails the build
- **WHEN** a produced installer does not contain qmtcode at its expected location
- **THEN** the desktop packaging job fails and the release publishes no desktop assets

### Requirement: Nix package provides qmtcode
The Nix `querymt-gpui` package SHALL provide the qmtcode of the same flake at `lib/querymt-gpui/qmtcode` in its output, so that the app finds it the same way as in the Linux packages.

#### Scenario: Nix package exposes qmtcode to the app
- **WHEN** the `querymt-gpui` flake package is built
- **THEN** its output contains `lib/querymt-gpui/qmtcode`, which runs the flake's qmtcode

### Requirement: App identity is QueryMT Desktop
Installed builds SHALL identify as "QueryMT Desktop". On macOS the bundle is `QueryMT Desktop.app` with identifier `mt.query.desktop`. On Windows the install directory and the uninstall entry are named "QueryMT Desktop". On Linux the deb package is `querymt-desktop` and the desktop entry is "QueryMT Desktop".

#### Scenario: macOS bundle identity
- **WHEN** the macOS bundle is built
- **THEN** it is named `QueryMT Desktop.app`, its bundle identifier is `mt.query.desktop` and its display name is "QueryMT Desktop"

#### Scenario: Deb replaces the earlier package name
- **WHEN** the `querymt-desktop` deb is installed on a system with the `querymt-gpui` package installed
- **THEN** the package manager replaces `querymt-gpui` with `querymt-desktop`

#### Scenario: Windows installer replaces an earlier GPUI install
- **WHEN** the setup exe runs for a user who has the earlier "QueryMT GPUI" install
- **THEN** the earlier install is removed and only "QueryMT Desktop" remains in the installed apps list

#### Scenario: User state survives the rename
- **WHEN** a user who used the app under its earlier name starts the renamed app
- **THEN** their agents and preferences are the same as before
