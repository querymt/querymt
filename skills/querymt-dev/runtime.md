# Isolated runtime lab

Use this when the behavior needs a running candidate. A fixture test is often
faster for pure logic. Never use the active development agent as the test subject.

## Prepare once, reuse for the task

- Identify running development processes, their executables, DB/config paths and
  ports so the test instance does not conflict with them. Inspect candidate help
  and select the profile that reproduces the behavior.
- Create a separate lab root, fixture workspace and unique job names. Use fresh
  HOME, config, database and ports; isolate relevant OS-specific cache/state paths.
  `--db` overrides `QMT_SESSIONS_DB`, then the config-dir default. Config-dir order
  is `QMT_CONFIG_DIR`, `QMT_HOME`, then `~/.qmt`. Profile/skill discovery can use
  HOME independently, so a DB override alone is not isolation.
- Use the provider, tools and configuration needed to reproduce the behavior.
  Prefer an existing deterministic provider fixture when the real provider is not
  relevant. Disable unrelated scheduled work only in the test configuration/copy.
- When existing data is needed, open the source SQLite database read-only and use
  online backup into a distinct lab database while the user's service keeps running.
  Never write to, migrate, checkpoint, replace or delete the source; do not `cp`
  live DB/WAL files or write test data back. Run migrations and reproductions only
  against the copy. Check copied workspace paths and schedules before test startup.
- Choose free loopback ports and a wall-clock budget. Keep OTLP disabled unless
  the check needs it and suitable tooling is available. Use captured logs or
  read-only database inspection otherwise; see [runtime evidence](telemetry.md).

## Build the actual candidate

Build in the [selected development environment](development.md) under the
development HOME so toolchain and dependency caches remain available; use the lab
HOME at runtime.

Choose an absolute `TARGET` not used by a running agent and reuse it for this
task. From the repo root, inside the selected environment:

```sh
env CARGO_TARGET_DIR="$TARGET" cargo check -p querymt-agent
env CARGO_TARGET_DIR="$TARGET" cargo build -p querymt-agent --example qmtcode
```

Add features only as the scenario requires, consistently across check/build/tests.
Do not overwrite an installed or running executable. Record source/dirty-patch and
binary identity; verify the launched process uses that binary before acceptance.

## Launch and supervise on the current host

The examples use Bash (including macOS's bundled Bash), in the selected development
environment. Set absolute `REPO`, candidate `BIN` and minimal fixture `CONFIG` paths,
and choose an unused loopback `PORT` first. Run only the applicable OS section;
keep the same shell variables through setup, checks and cleanup.

### Separate test state

```bash
: "${REPO:?set the working directory}" "${BIN:?set the candidate executable}"
: "${CONFIG:?set the fixture config}" "${PORT:?choose a free loopback port}"
LAB=$(mktemp -d "${TMPDIR:-/tmp}/qmt-lab.XXXXXX")
NAME="querymt-$(basename "$LAB")"
mkdir -p "$LAB"/{home,config,cache,data,state,tmp}
RUN=( "$(command -v env)" "PATH=$PATH"
  "HOME=$LAB/home" "QMT_CONFIG_DIR=$LAB/config"
  "QMT_SESSIONS_DB=$LAB/sessions.db"
  "XDG_CONFIG_HOME=$LAB/config" "XDG_CACHE_HOME=$LAB/cache"
  "XDG_DATA_HOME=$LAB/data" "XDG_STATE_HOME=$LAB/state" "TMPDIR=$LAB/tmp"
  "QMT_NO_TELEMETRY=1"
  "$BIN" "$CONFIG" --db "$LAB/sessions.db" --acp-ws "127.0.0.1:$PORT" )
```

`RUN` overrides the test-state paths without clearing the job's environment.
Systemd and launchd do not source the interactive shell, so add the provider and
runtime variables needed by the test to `RUN`. Put its provider configuration in
`$LAB/config`. For export tests, replace the telemetry-disable setting per
[telemetry](telemetry.md).

### Linux: transient systemd user service

Requires an available user systemd manager. The service is transient.
The ten-minute deadline covers the server, not the build. Adjust it to the test.

```bash
UNIT="$NAME.service"
systemd-run --user --collect --unit="$UNIT" --working-directory="$REPO" \
  --expand-environment=no --property=Type=exec --property=Restart=no \
  --property=RuntimeMaxSec=600 --property=TimeoutStopSec=5 \
  --property=KillMode=control-group -- "${RUN[@]}"
systemctl --user show "$UNIT" -p MainPID -p ExecStart -p ActiveState
journalctl --user -u "$UNIT" -n 50 --no-pager
```

Probe readiness and run the selected checks, then stop this unit and save its logs:

```bash
systemctl --user stop "$UNIT"
journalctl --user -u "$UNIT" --no-pager > "$LAB/service.log"
```

`--collect` may remove the unit after exit; use its journal if status is gone.
Check installed `systemd-run --help` for option support. If no user manager exists,
use an available managed job tool with equivalent deadline and child cleanup.

### macOS: temporary launchd job

Keep the plist in the lab, not `~/Library/LaunchAgents`, so it is not loaded on future
logins. This noninteractive wrapper waits for the candidate and exits on deadline;
launchd then cleans remaining processes in the job's process group, including the
watchdog. Keep `AbandonProcessGroup` false and do not let test children daemonize.
Timeout cleanup is not evidence of graceful application shutdown.

```bash
cat > "$LAB/deadline.sh" <<'SH'
#!/bin/sh
set -eu
budget=$1; shift
parent=$$
trap 'exit 124' USR1
trap 'exit 143' TERM INT
(sleep "$budget" && kill -USR1 "$parent") &
"$@" &
wait "$!"
SH
DOMAIN="gui/$(id -u)"
launchctl print "$DOMAIN" >/dev/null # Verify the login domain before loading.
PLIST="$LAB/$NAME.plist"
JOB="$DOMAIN/$NAME"
plutil -create xml1 "$PLIST"
plutil -insert Label -string "$NAME" "$PLIST"
plutil -insert ProgramArguments -array "$PLIST"
i=0
for arg in /bin/sh "$LAB/deadline.sh" 600 "${RUN[@]}"; do
  plutil -insert "ProgramArguments.$i" -string "$arg" "$PLIST"
  i=$((i + 1))
done
plutil -insert WorkingDirectory -string "$REPO" "$PLIST"
plutil -insert RunAtLoad -bool true "$PLIST"
plutil -insert KeepAlive -bool false "$PLIST"
plutil -insert AbandonProcessGroup -bool false "$PLIST"
plutil -insert ExitTimeOut -integer 5 "$PLIST"
plutil -insert StandardOutPath -string "$LAB/stdout.log" "$PLIST"
plutil -insert StandardErrorPath -string "$LAB/stderr.log" "$PLIST"
plutil -lint "$PLIST" && launchctl bootstrap "$DOMAIN" "$PLIST"
launchctl print "$JOB"
```

For a headless session without a GUI domain, verify `user/$(id -u)` exists and use
that domain instead. Stop on setup errors.
`plutil` encodes argument strings safely, including paths with spaces or `&`.
`ExitTimeOut` is only the shutdown grace period; the wrapper supplies the actual
600-second runtime limit. `KeepAlive=false` prevents automatic restart on failure.

After readiness and the selected checks, inspect logs and remove only this job:

```bash
tail -n 50 "$LAB/stdout.log" "$LAB/stderr.log"
launchctl bootout "$JOB"
```

Use the full service target for `bootout`, never the bare domain. Confirm the job
is no longer registered and its processes/port are gone; keep the test logs.

For either OS, process started is not server ready. Probe via the existing client
skill as described in [ACP checks](acp.md), and record job/PID, executable, endpoint
and state ownership. Read [mesh checks](mesh.md) before starting mesh; separate DB
state alone does not isolate network identity.

## Cleanup and restart

Never stop or restart user-run QueryMT services or pre-existing `qmtcode` processes,
including to free a port, copy a database or recover a failed test. Choose another
test port or report the blocker instead. Before faulting/stopping a process, verify
its job handle and current PID/executable identify a process created for this test.
If ownership is unclear, pause and ask. Stop only those test jobs, verify ports
freed and children stopped, and preserve evidence. Never `killall`, operate an
unknown service, or trust an old PID file alone. Reuse test identity/DB only for a continuity test; start
fresh for a clean reproduction. Observer restart and owner restart are different
tests; do not blindly resubmit a prompt after either.
