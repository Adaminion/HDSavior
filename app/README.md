# HDSavior app

The Windows desktop UI of HDSavior (Flutter). It does no disk access of its
own: it starts `hdsavior-engine.exe serve` as a child process and talks to it
in JSON lines, as specified in [docs/PROTOCOL.md](../docs/PROTOCOL.md). The
overall design is in [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) (sections
2, 4 and 6 concern the UI).

This directory currently holds the **foundation** of the app: the project and
Windows runner, the engine layer, the application state, a fake engine for
tests, and a shell with placeholders for the five screens. The screens
themselves are the next step; everything they need is described below.

- [Build and run](#build-and-run)
- [Running without the real engine](#running-without-the-real-engine)
- [Architecture](#architecture)
- [Engine layer](#engine-layer-libengine)
- [State layer](#state-layer-libstate)
- [Tests](#tests)
- [Dependencies](#dependencies)

## Build and run

Requirements: Windows 10 or 11, Flutter 3.50.1 (Dart 3.14), Visual Studio 2022
with the "Desktop development with C++" workload, and a Rust toolchain for the
engine.

```powershell
# 1. The engine (once, and whenever it changes)
cd engine
cargo build --release              # -> engine\target\release\hdsavior-engine.exe

# 2. The app
cd ..\app
flutter pub get
flutter build windows              # -> build\windows\x64\runner\Release\hdsavior.exe
```

**`flutter run` must be started from an elevated terminal** ("Run as
administrator"). The app's manifest requests administrator rights
(`requireAdministrator`), because reading raw disks needs them; Windows
refuses to start such a program from a process that is not elevated itself
(error 740, "The requested operation requires elevation"), and `flutter run`
starts the program directly. Double-clicking `hdsavior.exe` shows the UAC
prompt instead.

### How the engine gets into the bundle

`windows/CMakeLists.txt` has an install rule that copies the engine next to
`hdsavior.exe` on every `flutter build windows` / `flutter run`. It takes the
first of these that exists:

1. the file named by the environment variable `HDSAVIOR_ENGINE_EXE`
   (whatever its file name; it is installed as `hdsavior-engine.exe`),
2. `..\engine\target\release\hdsavior-engine.exe`.

If neither exists the build still succeeds and prints a CMake warning; the app
then starts and explains that the engine was not found.

### How the app finds the engine at run time

`locateEngineExecutable()` (lib/engine/engine_locator.dart): the value of the
environment variable **`HDSAVIOR_ENGINE`** if it is set, otherwise
`hdsavior-engine.exe` in the directory of the app's own executable.

### What was changed in the generated Windows runner

- `runner/runner.exe.manifest`: `requestedExecutionLevel
  level="requireAdministrator"`. `runner/CMakeLists.txt` passes
  `/MANIFESTUAC:NO` to the linker so that its default `asInvoker` fragment does
  not collide with the manifest (two different levels cannot be merged:
  mt.exe error c1010001).
- `runner/main.cpp`: window title `HDSavior`; default size 1280x800 logical
  pixels, shrunk to the work area and centred on the primary monitor; minimum
  size 960x600.
- `runner/win32_window.{h,cpp}`: `SetMinimumSize` and the `WM_GETMINMAXINFO`
  handler (scaled by the monitor's DPI, never larger than its work area).
- `runner/Runner.rc`: product name and file description `HDSavior` (the name
  the UAC prompt shows).
- `CMakeLists.txt`: the engine install rule.

Only the `windows/` platform folder is committed. `linux/` is git-ignored.

## Running without the real engine

`test/fake_engine/` holds a fake engine written in Dart: the whole protocol
over a generated world (seven disks, a tree of up to several hundred thousand
nodes with every rating and flag, five file systems, jobs with progress,
refusals and failures). It is what the tests run against, and it can drive
the app:

```powershell
flutter run -d windows -t test/fake_engine/run_app.dart
```

It can also stand in for the engine executable (it speaks the protocol on its
standard streams): `dart test/fake_engine/serve.dart serve`, wrapped in a
script that `HDSAVIOR_ENGINE` points to.

Behaviour is selected by names, so it can be triggered from the running app.
The full table is at the top of `test/fake_engine/fake_world.dart`; the most
useful entries:

| type this                                  | to get                                   |
|--------------------------------------------|------------------------------------------|
| destination on `E:\` (the rescued disk)    | `overlaps_source` (always refused)       |
| destination on `S:\` / `U:\`               | `same_disk` / `unknown_disk` (consent)   |
| destination on `Q:\`, `R:\`, `relative`    | `not_found`, `not_writable`, `invalid_path` |
| destination on `T:\` / `K:\`               | only 1 MiB free / FAT32 (4 GiB limit)    |
| image file `...\missing.img`, `denied.img` | `not_found`, `access_denied`             |
| session file `...\detached.hdsc`           | loads without its disk (browse only)     |
| session file `...\partial.hdsc`            | a stopped scan that can be resumed       |
| image destination `...\exists.img`         | `exists`                                 |

On Linux (the app itself is Windows-only, but everything except the runner
runs anywhere): make a scratch copy, add the platform folder there, and run
under Xvfb.

```sh
cp -r app /tmp/hdsavior-app && cd /tmp/hdsavior-app
flutter create --platforms=linux --org com.hdsavior --project-name hdsavior .
flutter build linux --debug -t test/fake_engine/run_app.dart
xvfb-run -a build/linux/x64/debug/bundle/hdsavior
```

## Architecture

```
lib/
  main.dart            entry point: process transport + platform services
  app.dart             HdSaviorApp: MaterialApp, starts the engine, shuts it down on exit
  engine/              the protocol as Dart types; no Flutter imports
  state/               application state: ChangeNotifiers on top of the engine layer
  platform/            PlatformServices (OS-specific bits) and FilePickers (dialogs), both interfaces
  ui/                  the shell (navigation, permanent notice, engine status) and screens
test/
  fake_engine/         the fake engine (pure Dart) and its transport
  support/             FakeApp (state layer on the fake), ScriptedTransport (byte-level),
                       stand-ins for the OS services and the dialogs
  data/                the JSON examples of PROTOCOL.md, verbatim
  engine/ state/ ui/ platform/   the tests
```

Rules the code follows:

- **The UI holds no file data.** Rows are fetched from the engine in pages on
  demand and kept in a bounded cache; a folder with a million files costs a
  few pages of memory. Checking a folder stores one id.
- **The engine is the authority.** The app never decides by itself whether a
  destination is safe, what is selected in a subtree, or whether a job may
  start; it asks, and shows the engine's answer and message.
- **Nothing in `lib/` uses `dart:ffi` or Win32.** OS-specific behaviour
  (default session directory, opening a folder) is behind `PlatformServices`
  and built on `dart:io` only, so tests and the app itself run on Linux. The
  file and folder dialogs are behind `FilePickers` (`app.pickers`:
  `pickFile`, `pickSavePath`, `pickFolder`), implemented with the
  `file_selector` plugin. Tests use the stand-ins of
  `test/support/fake_platform.dart` (`ScriptedFilePickers`,
  `InertPlatformServices`).
- **The app writes nothing of its own into the session range** (ARCHITECTURE
  S2): whenever a source is opened it asks `check_destination` with purpose
  `app_data` for its session directory and creates it only on verdict `ok`
  (`SourceController.canWriteAppData`, `ensureSessionDirectory`).
- **State management is the SDK's `ChangeNotifier` / `ValueNotifier`**, no
  package. The state is a handful of long-lived controllers created in one
  place (`AppController`); widgets listen with `ListenableBuilder`. That is
  all the app needs, it keeps the controllers plain Dart objects that the
  tests drive without widgets, and it adds no dependency.

### What a screen does

```dart
class ScanScreen extends StatelessWidget {
  const ScanScreen({super.key, required this.app});
  final AppController app;

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: app.scan,                       // any controller is a Listenable
    builder: (context, _) => Text('${app.scan.phase} ${app.scan.progress?.fraction}'),
  );
}
```

Calls that talk to the engine return futures that fail with an
`EngineException`; `e.kind` is the typed error code and `e.message` the
engine's plain-language text:

```dart
try {
  await app.recover.start(dest: path);
} on EngineException catch (e) {
  // e.kind == EngineErrorCode.destinationRefused: app.recover.destinationCheck
  // now holds the verdict and its message. EngineErrorCode.busy, noSpace, ...
}
```

`AppController.screen` (a `ValueNotifier<AppScreen>`) says which of the five
screens the shell shows. The shell (`lib/ui/app_shell.dart`) already provides
the navigation rail, the permanent safety notice (`SafetyNotice`, on every
screen) and the view for an engine that is starting, failed or incompatible.
The files in `lib/ui/screens/` are placeholders to be replaced one by one.

## Engine layer (`lib/engine`)

Import `package:hdsavior/engine/engine.dart`.

| class | role |
|---|---|
| `EngineTransport`, `EngineConnection` | Interface to "something that starts an engine and gives its standard streams". `ProcessEngineTransport` is the real one (`hdsavior-engine serve`); tests use `FakeEngineTransport` and `ScriptedTransport`. |
| `EngineClient` | Owns the process. Frames JSON lines (UTF-8, `LineSplitter`; lines of many megabytes are fine), matches responses to requests by id, publishes events (`events`, `on<T>()`), reports the end of the process (`exits`) and fails pending requests with `engine_exited`. `start`, `restart`, `shutdown` (shutdown command, end of input, kill as last resort), `kill`. |
| `EngineApi` | One typed method per protocol command: `hello`, `listDisks`, `openSource`, `closeSource`, `scanStart`, `jobCancel`, `sessionSave`, `sessionLoad`, `sessionAttachSource`, `catalogInfo`, `geometrySet`, `listChildren`, `nodeInfo`, `search`, `selectionSummary`, `checkDestination`, `recoverStart`, `carveStart`, `imageStart`, `readFile`, `shutdown`; plus `readFileContent` (a file's first N bytes in small chunks, cancellable: for previews). |
| `EngineException` | The one exception type of failed requests: `code`/`kind`, `message`, `data`, `command`; `destinationCheck` for `destination_refused`; `isEngineGone`. Client-side kinds: `engineExited`, `engineNotRunning`, `engineStartFailed`, `protocolError`. |
| `CancelToken`, `RequestCancelledException` | Client-side cancellation of requests (see below). |
| objects.dart | `SourceInfo`, `Partition`, `NtfsBootInfo`, `Disk`, `Warning`, `Geometry`, `Instance`, `InstanceVirtual`, `LiveVolume`, `ByteRange`, `CatalogInfo`, `NodeRow` (PROTOCOL `Row`), `FileFilter` (PROTOCOL `Filter`), `EngineError`. |
| results.dart | `HelloResult`, `OpenSourceResult`, `ScanStartResult`, `SessionSaveResult`, `SessionLoadResult`, `SessionAttachResult`, `RowsPage`, `NodeInfo` (`NodeRef`, `RecordInfo`, `DataInfo`, `FlagDetail`), `SelectionSummary`, `DestinationCheck`, `RecoverStartResult`, `CarveStartResult`, `ImageStartResult`, `ReadFileResult`, `SourceRef`. |
| events.dart | `EngineEvent` (sealed): `ScanProgress`, `ScanDone`, `RecoverProgress`, `RecoverDone`, `CarveProgress`, `CarveDone`, `ImageProgress`, `ImageDone` (grouped as `JobProgressEvent` / `JobDoneEvent`), `TaskProgress`, `LogEvent`, `UnknownEvent`, `MalformedEvent`. |
| enums.dart | Every closed set of strings, each value with its wire name (`.wire`). |

Conventions of the models:

- Immutable; `fromJson(JsonObject)` ignores unknown members and throws a
  `ProtocolFormatException` naming the member (`list_children.rows[3].size: is
  missing`) when a documented member is missing or mistyped. Through
  `EngineApi` that surfaces as an `EngineException` of kind `protocolError`.
- Byte counts, offsets and ids are Dart `int`s (64-bit: the app only runs on
  the VM). Timestamps stay Unix milliseconds (`int?`), with `timeFromMs` and
  `NodeRow.modifiedTime` for display.
- A string this version does not know (a rating, flag, warning code or error
  code of a newer engine) parses to the enum's `unknown` value; the original
  text stays available (`Warning.code`, `NodeRow.flagNames`,
  `EngineException.code`).
- Names that differ from the document: `Row` is `NodeRow` (Flutter has a
  `Row`), `Filter` is `FileFilter`, `offset_source: "index"` is
  `OffsetSource.indexVotes` and `"default"` is `byDefault` (reserved words).

Requests and cancellation: the engine handles requests strictly one after the
other, so the client keeps one request at the engine and queues the rest
itself. A request made with a `CancelToken` that is cancelled while it is
still queued is never sent; one that was already sent completes at once with
`RequestCancelledException` and its answer is dropped. `jobCancel` jumps the
queue.

## State layer (`lib/state`)

Import `package:hdsavior/state/state.dart`. `AppController` creates and wires
everything; its fields are the public surface for the screens.

| field | class | what it holds |
|---|---|---|
| `engine` | `EngineController` | `status` (`stopped`, `starting`, `ready`, `incompatible`, `failed`), `hello`, `error`, `lastExit` (with the tail of the engine's log); `start`, `restart`, `shutdown`. |
| `source` | `SourceController` | `disks` / `refreshDisks`; `current` (`OpenedSource`: info, table, partitions, warnings, `attached`, `attachError`); `openDisk`, `openImage`, `close`, `loadSession`, `attachSource`, `saveSession`; `sessionPath`; `appDataCheck`, `canWriteAppData`, `ensureSessionDirectory`, `suggestSessionPath`. |
| `catalog` | `CatalogController` | `info` (`CatalogInfo`), `epoch`, `sourceAttached`; `refresh`, `guarded`, `nodeInfo`, `readContent`, `setGeometry`; owns `selection` and `rows`. |
| `selection` | `SelectionModel` | The include/exclude sets: `setChecked`, `toggle`, `stateOf`, `isSelected`, `clear`, `include`, `exclude`, `spec`. |
| `catalog.rows` | `RowCache` | `open(RowQuery)` -> `PagedRows`. |
| `browse` | `BrowseController` | `folder` (`FolderLocation`), `filters` (`ViewFilters`), `sort`, `descending`, `sortBy`, `open`, `openRoot`; `rows` (the table), `folders(node)` (the tree), `ancestorsOf(row)`. |
| `scan` | `ScanController` | `start`, `resume`, `stop`; `scanPhase`, `canStop`; plus the job members below. |
| `recover` | `RecoverController` | `options` (`RecoverOptions`), `summary` (follows the selection), `checkDestination`, `destinationCheck`, `allows`, `start`; plus the job members. |
| `carve` | `CarveController` | `hasCarveHits`, `start(kinds)`; plus the job members. |
| `image` | `ImageController` | `checkDestination`, `destinationCheck`, `start`; plus the job members. |
| `jobs` | `JobTracker` | `value`: which job is active (`JobKind?`); the engine runs one at a time. |
| `task` | `ValueNotifier<TaskProgress?>` | Progress of a long plain request (`session_load`, `geometry_set`). |
| `screen` | `ValueNotifier<AppScreen>` | The screen the shell shows. |
| `platform` | `PlatformServices` | `defaultSessionDirectory`, `createDirectory`, `openFolder` (show the recovered files in Explorer). |
| `pickers` | `FilePickers` | The open, save and folder dialogs. |
| `api`, `client` | `EngineApi`, `EngineClient` | Direct access to the engine for what no controller wraps (for example `checkDestination` with purpose `session`), and the `log` events (`app.log`). |

### Jobs

`ScanController`, `RecoverController`, `CarveController` and `ImageController`
extend `JobController<Start, Progress, Done>`:

- `phase`: `idle` -> `starting` -> `running` (-> `cancelling`) -> `done`.
- `started` (the answer to `*_start`), `progress` (the latest progress event),
  `result` (the done event), `status` (`completed`, `stopped`, `cancelled`,
  `failed`), `failure` (why it failed: the done event's error, or
  `engine_exited` / `protocol_error`), `startError` (why a start was refused;
  for `destination_refused` its `destinationCheck` says why).
- `cancel()` sends `job_cancel`; the job then ends with its done event.
  `reset()` dismisses a finished job.

What happens around a job is wired already: a scan clears the catalog when it
is accepted and adopts the new one when it is done; carving keeps the epoch
and drops ticked carved files; a recovery refreshes the rows afterwards;
`source_lost` marks the source as no longer attached; the death of the engine
ends the job with `engine_exited`.

### Catalog, epochs, rows

Node ids are valid for one catalog epoch. `CatalogController` enforces that on
the client side: when a catalog with a new epoch arrives (or the catalog goes
away) the selection is cleared and every `PagedRows` is closed; when the epoch
stays but rows may have changed (carving, a new geometry, a recovery) the rows
are fetched again and the selection is kept. A `stale_catalog` answer to any
request made through `catalog.guarded` triggers `catalog.refresh()`.

A list widget uses a `PagedRows` like this:

```dart
ListenableBuilder(
  listenable: Listenable.merge([app.browse, app.selection]),
  builder: (context, _) {
    final rows = app.browse.rows;               // null without a catalog
    if (rows == null) return const SizedBox();
    return ListenableBuilder(
      listenable: rows,                         // rebuilds when pages arrive
      builder: (context, _) {
        rows.load();                            // learn `total` if unknown
        return ListView.builder(
          itemCount: rows.total ?? 0,
          itemExtent: 28,
          itemBuilder: (context, index) {
            final row = rows.rowAt(index);      // null while its page loads
            if (row == null) return const SizedBox(height: 28);
            final ancestors = app.browse.ancestorsOf(row);
            return CheckboxListTile(
              tristate: true,
              value: switch (app.selection.stateOf(row.id, ancestors)) {
                CheckState.checked => true,
                CheckState.unchecked => false,
                CheckState.partial => null,
              },
              onChanged: (_) => app.selection.toggle(row.id, ancestors),
              title: Text(row.name),
            );
          },
        );
      },
    );
  },
)
```

`rowAt` may be called during a build; it never notifies synchronously. Pages
have a fixed size (200 rows), at most 64 pages are kept across all lists
(least recently used first out), and requests that scrolling has left behind
are cancelled. A page that failed is not requested again until `retry()`;
`rows.error` says why.

`BrowseController.rows` is the table: the children of the current folder
(`list_children`), or, as soon as the filters contain a name or a size, the
matching files of the whole subtree (`search`; those rows carry `path` and
`ancestorIds`). `BrowseController.folders(node)` is one level of the tree
(folders only). Both are replaced when what they show changes.

### Selection

`SelectionModel` is exactly PROTOCOL's rule: *a node is selected when its
nearest ancestor-or-self that appears in `include` or `exclude` is in
`include`*. The model never needs a folder's content; what it needs for every
node it is asked about is the node's **ancestor ids, root first, without the
node itself**: `FolderLocation.childAncestors` for rows of a folder listing
(`app.browse.ancestorsOf(row)` does it), `NodeRow.ancestorIds` for search
rows, `NodeInfo.ancestorIds`, `const []` for an instance root. The sets are
kept minimal, so `stateOf` (tri-state) costs O(depth) and checking a folder is
one entry however much it holds. `RecoverController.summary` follows the
selection and the options automatically.

### Destinations

`recover.checkDestination(path)` / `image.checkDestination(path)` ask the
engine's destination guard and keep the verdict; `recover.allows(check)` says
whether a start can succeed with the consents in `recover.options`
(`allowSameDisk`, `allowUnknownDisk`; `overlaps_source` can never be allowed).
The engine checks again at start and its refusal lands in `destinationCheck`
as well. Paths are typed or come from `app.pickers`; UNC paths are allowed
(mapped drive letters are invisible to an elevated program).

### When the engine dies

`engine.status` becomes `failed`, `engine.lastExit` carries the exit code and
the tail of the engine's log, running jobs end with `engine_exited`, and
source, catalog, selection and rows are reset (the engine's state died with
it). `app.restartEngine()` starts a fresh one; `source.sessionPath` still
names the last session file, which can be loaded again.

## Tests

```sh
flutter analyze        # must report no issues
flutter test           # must pass; two tests report "skipped" until the real engine is there
```

| where | what it proves |
|---|---|
| `test/engine/protocol_examples_test.dart` | Every JSON example of PROTOCOL.md (verbatim copies in `test/data/protocol_examples.txt`, checked against the document for drift in both directions) parses; every documented member is read and has the documented value; unknown members are ignored; any missing or mistyped member gives a `ProtocolFormatException` and nothing else. |
| `test/engine/engine_api_test.dart` | Each API method sends the documented command with exactly the documented argument names and defaults. |
| `test/engine/engine_client_test.dart` | Framing (split at every byte, merged chunks, an 11 MB line), id matching, event order, error mapping, cancellation, engine exit, restart, shutdown and kill. |
| `test/engine/process_transport_test.dart` | The real process transport against the fake engine running as a child process. |
| `test/engine/protocol_transcript_test.dart` | Reads `docs/protocol-examples.jsonl` (the transcript the real engine records) as soon as it exists. |
| `test/engine/engine_conformance_test.dart` | One whole conversation (open an image, scan, browse, summarise, read, recover, shut down) asserting only what PROTOCOL.md guarantees. Always against the fake engine as a process; against the real engine when `HDSAVIOR_ENGINE` and `HDSAVIOR_TEST_IMAGE` are set. |
| `test/fake_engine/fake_engine_test.dart` | The fake itself: the errors-by-state table, epochs, sort order, filters, aggregates, selection semantics against brute force, jobs. |
| `test/state/*` | The selection model against a naive model on random trees; the row cache; every controller and job against the fake engine, step by step. |
| `test/platform/*` | Session directory, opening a folder, the dialogs. |
| `test/ui/app_shell_test.dart` | The shell: notice, navigation, engine failure, clean shutdown on exit. |

To run the conformance conversation against the real engine (Linux or
Windows):

```sh
HDSAVIOR_ENGINE=../engine/target/release/hdsavior-engine \
HDSAVIOR_TEST_IMAGE=../fixtures-out/basic.img \
flutter test test/engine/engine_conformance_test.dart
```

Writing tests for screens: use `FakeApp` (`test/support/fake_app.dart`). The
fake engine's jobs advance only when the test calls `fake.step()` /
`fake.finishJob()`, so every intermediate state can be asserted. The dialogs
answer from `fake.pickers.answers`.

In a widget test (`testWidgets`) time is faked, and a future from the engine
only completes while the test pumps. **Never `await` an engine call directly
there**; it would wait for ever. Create the fake with the tester's pump and
wrap calls in `fake.run`:

```dart
testWidgets('...', (tester) async {
  final fake = FakeApp(settle: () => tester.pump(const Duration(milliseconds: 10)));
  await tester.pumpWidget(HdSaviorApp(controller: fake.app));
  await fake.settle();                             // engine started, disks listed
  await fake.openDisk();
  await fake.run(() => fake.app.scan.start());     // not: await fake.app.scan.start()
  await fake.step(3);
  expect(find.textContaining('sweep'), findsOneWidget);
  await tester.pumpWidget(const SizedBox());       // unmount before disposing
  await fake.dispose();
});
```

## Dependencies

- `material_ui`: the Material library as a package. `package:flutter/material.dart`
  is deprecated in its favour since Flutter 3.47, and the project template of
  this SDK depends on it.
- `file_selector` 1.1.0 (the current version; requires Dart 3.9 / Flutter
  3.35 or newer): the official plugin for the open, save and folder dialogs,
  used by `SystemFilePickers`. Its Windows implementation is
  `file_selector_windows`.
- Dev: `flutter_test`, `flutter_lints`, and `file_selector_platform_interface`
  (already a transitive dependency; one test replaces the dialogs with it).

No state-management, JSON-codegen or utility packages: the models are written
by hand (about one line per member, with path-aware error messages that
generated code would not give), and the state uses the SDK's notifiers.
