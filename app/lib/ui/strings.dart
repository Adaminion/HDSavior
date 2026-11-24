/// Every text the user sees, in one place, so that the app can be
/// translated later. Texts with values are functions.
///
/// Tone: calm, plain and honest. The user has just lost files; say what is
/// happening and what to do next, avoid jargon, never promise what the
/// engine has not confirmed. Technical terms appear only in the expert views
/// (geometry evidence, record facts) and behind "Technical details".
library;

import '../engine/engine.dart';
import 'results/file_types.dart';

abstract final class S {
  // --- app and shell ---------------------------------------------------------

  static const String appName = 'HDSavior';
  static const String navSource = 'Source';
  static const String navScan = 'Scan';
  static const String navResults = 'Results';
  static const String navRecover = 'Recover';
  static const String navImage = 'Image';

  static const String safetyNotice =
      'HDSavior only reads from the disk you are rescuing and never writes '
      'to it. Do not use that disk for anything else until your files are '
      'safe: every new file on it can destroy the old ones for good.';

  static const String closingTitle = 'Closing HDSavior…';
  static const String closingText = 'Saving the scan and stopping the engine.';
  static const String startingTitle = 'Starting…';
  static const String startingText = 'HDSavior is starting its disk engine.';
  static const String incompatibleTitle = 'The disk engine does not match this app';
  static String incompatibleText(String engine, int? protocol) =>
      'The engine ($engine) speaks protocol version $protocol, which this '
      'version of HDSavior does not understand. Install HDSavior again.';
  static const String engineFailedTitle = 'The disk engine is not running';
  static const String engineFailedFallback = 'The engine could not be started.';
  static const String engineFailedReassurance = 'Nothing on your disk was changed.';
  static String engineLastLog(String line) => 'Last message of the engine: $line';
  static const String engineRestart = 'Start the engine again';
  static const String engineFailedSession =
      'Your scan was saved to its session file. After the engine has started '
      'again, open it from the Source screen to continue where you were.';

  static String taskTitle(TaskKind kind) => switch (kind) {
    TaskKind.sessionLoad => 'Opening the saved scan…',
    TaskKind.geometrySet => 'Applying the disk layout…',
    TaskKind.unknown => 'Working…',
  };
  static const String taskText = 'This can take a while for a large disk. Please wait.';

  // --- common words and actions ---------------------------------------------

  static const String cancel = 'Cancel';
  static const String close = 'Close';
  static const String ok = 'OK';
  static const String retry = 'Try again';
  static const String browse = 'Browse…';
  static const String back = 'Back';
  static const String technicalDetails = 'Technical details';
  static const String copyPath = 'Copy path';
  static const String pathCopied = 'Path copied to the clipboard.';
  static const String loading = 'Loading…';
  static const String yes = 'Yes';
  static const String no = 'No';
  static const String unknown = 'unknown';
  static const String none = 'none';
  static const String showAll = 'Show all';
  static const String copy = 'Copy';

  // --- units (format.dart puts the numbers in) ----------------------------------

  static String bytesCount(String count, int n) => n == 1 ? '1 byte' : '$count bytes';

  /// Binary units with the names Windows uses.
  static const List<String> sizeUnits = ['KB', 'MB', 'GB', 'TB', 'PB', 'EB'];
  static String size(String number, String unit) => '$number $unit';
  static String perSecond(String size) => '$size/s';
  static String percent(String number) => '$number %';
  static String seconds(int s) => '$s s';
  static String minutesSeconds(int m, String s) => '$m min $s s';
  static String hoursMinutes(int h, String m) => '$h h $m min';
  static String daysHours(int d, int h) => '$d d $h h';

  static String files(String count, int n) => n == 1 ? '$count file' : '$count files';
  static String folders(String count, int n) => n == 1 ? '$count folder' : '$count folders';

  // --- errors -----------------------------------------------------------------

  /// A plain-language headline for an engine error.
  static String errorHeadline(EngineErrorCode code) => switch (code) {
    EngineErrorCode.badRequest =>
      'The engine did not accept this request. This is a fault in HDSavior, not in your disk.',
    EngineErrorCode.unknownCommand =>
      'The disk engine does not know this request. The app and the engine may not belong together.',
    EngineErrorCode.busy => 'Another task is still running. Wait for it to finish, or stop it first.',
    EngineErrorCode.noSource => 'No disk or disk image is connected for this.',
    EngineErrorCode.noSession => 'There is no scan to continue.',
    EngineErrorCode.noCatalog => 'There are no scan results yet.',
    EngineErrorCode.staleCatalog => 'The results changed in the meantime and were reloaded. Please try again.',
    EngineErrorCode.notFound => 'This could not be found.',
    EngineErrorCode.ioError => 'Reading or writing failed.',
    EngineErrorCode.accessDenied =>
      'Windows refused access. HDSavior needs administrator rights to read a disk directly.',
    EngineErrorCode.destinationRefused => 'This location cannot be used.',
    EngineErrorCode.sourceMismatch => 'This is not the disk (or image) that was scanned.',
    EngineErrorCode.sourceLost =>
      'The disk stopped responding. It may have been unplugged, or its cable or power supply may be loose.',
    EngineErrorCode.noSpace => 'The destination is full.',
    EngineErrorCode.exists => 'A file with this name already exists.',
    EngineErrorCode.unsupported => 'This is not supported.',
    EngineErrorCode.cancelled => 'Cancelled.',
    EngineErrorCode.internal => 'The disk engine ran into an internal error.',
    EngineErrorCode.engineExited => 'The disk engine stopped.',
    EngineErrorCode.engineNotRunning => 'The disk engine is not running.',
    EngineErrorCode.engineStartFailed => 'The disk engine could not be started.',
    EngineErrorCode.protocolError =>
      'The disk engine answered in a way this app does not understand. The app and the engine may not belong together.',
    EngineErrorCode.unknown => 'Something went wrong.',
  };

  static String errorCode(String code) => 'Code: $code';
  static String errorCommand(String command) => 'Request: $command';
  static String errorEngineMessage(String message) => 'Engine: $message';
  static const String errorTitle = 'This did not work';
  static const String unexpectedError = 'Something unexpected went wrong in HDSavior.';

  // --- source warnings and catalog warnings ---------------------------------

  /// A short headline for a warning code; the engine's own message follows
  /// it.
  static String warningHeadline(WarningCode code) => switch (code) {
    WarningCode.trimSupported => 'This device may have erased the data when it was formatted',
    WarningCode.mountedVolume => 'Windows is using this disk right now',
    WarningCode.pageFileOnSource => 'Windows keeps a page file on this disk',
    WarningCode.systemDisk => 'This is the disk Windows runs from',
    WarningCode.appOnSource => 'HDSavior itself runs from this disk',
    WarningCode.sourceReadsZero => 'The disk reads back as empty',
    WarningCode.carveHitsCapped => 'Too many file signatures to keep',
    WarningCode.liveVolumeIncomplete => 'The new file system could not be read completely',
    WarningCode.writesNotDetectable => 'Writes since the format cannot be detected',
    WarningCode.scanIncomplete => 'The scan did not read the whole range',
    WarningCode.outOfRangeFiles => 'Some files lie outside the scanned area',
    WarningCode.identityDiffers => 'The disk reports a different model or serial number',
    WarningCode.mixedGenerations => 'Some records of an older file system could not be separated',
    WarningCode.sessionFileAtRisk => 'The scan progress file is inside the lost file system',
    WarningCode.unknown => 'Note from the disk engine',
  };

  /// What the user should do about a warning, when there is something to do.
  static String? warningAdvice(WarningCode code) => switch (code) {
    WarningCode.trimSupported =>
      'SSDs and many newer hard disks discard everything on format. If so, nothing can be recovered. '
          'A scan will tell: if it finds nothing, the data is gone.',
    WarningCode.mountedVolume =>
      'Do not save, copy or install anything on it. Close programs that use it. Better still, '
          'take the volume offline in Disk Management before you continue.',
    WarningCode.pageFileOnSource =>
      'Windows writes to the page file all the time, which overwrites old data on that disk.',
    WarningCode.systemDisk =>
      'Windows writes to this disk all the time. If possible, shut down and rescue it from another computer.',
    WarningCode.appOnSource =>
      'Running a program from the disk you rescue writes to it. Install HDSavior on another disk.',
    WarningCode.sourceReadsZero =>
      'The first gigabyte contains only zeros. This happens after a full format or on devices that erase on format.',
    WarningCode.carveHitsCapped => 'The search by signature will cover part of the disk only.',
    WarningCode.liveVolumeIncomplete =>
      'Files written to the disk after the format may not all be recognised as overwritten. '
          'Check the files you recover.',
    WarningCode.writesNotDetectable =>
      'HDSavior cannot tell which old files were overwritten after the format. '
          'Check the files you recover.',
    WarningCode.scanIncomplete => 'Continue the scan to find the rest.',
    WarningCode.outOfRangeFiles =>
      'The lost file system was larger than what was scanned. Scan the whole disk to reach these files.',
    WarningCode.identityDiffers =>
      'This is normal for some USB adapters. The contents were checked and match the scan.',
    WarningCode.mixedGenerations =>
      'Some files may be shown with an older version. Their condition is marked as uncertain.',
    WarningCode.sessionFileAtRisk => 'Save the scan to another disk, or the next save could overwrite lost files.',
    WarningCode.unknown => null,
  };

  // --- ratings and flags -------------------------------------------------------

  static String rating(Rating? rating) => switch (rating) {
    Rating.good => 'Good',
    Rating.uncertain => 'Uncertain',
    Rating.damaged => 'Damaged',
    Rating.unsupported => "Can't recover",
    Rating.unknown => 'Unknown',
    null => '',
  };

  static String ratingExplanation(Rating? rating) => switch (rating) {
    Rating.good => 'Everything indicates that this file will come back intact.',
    Rating.uncertain => 'The file can be recovered, but something about it is not certain. Check it after recovery.',
    Rating.damaged => 'Part of this file is missing or was overwritten. What is left can still be recovered.',
    Rating.unsupported =>
      'HDSavior cannot recover this file (for example it is compressed or encrypted by Windows, '
          'or it lies inside a disk image file).',
    Rating.unknown || null => '',
  };

  /// A plain-language explanation of a flag, used when the engine sent none.
  static String flag(FileFlag flag) => switch (flag) {
    FileFlag.deleted => 'This file had already been deleted before the format.',
    FileFlag.tornRecord => 'Its file record was only partly written; some details may be wrong.',
    FileFlag.missingFragments => 'Parts of the file could not be located.',
    FileFlag.sizeEstimated => 'The exact size is unknown; it was estimated.',
    FileFlag.outOfBounds => 'Part of the file lies outside the scanned area of the disk.',
    FileFlag.overwritten => 'Part of the file was overwritten after the format.',
    FileFlag.overlap => 'Another file claims some of the same space on the disk.',
    FileFlag.compressed => 'Windows stored this file compressed; HDSavior cannot decompress it.',
    FileFlag.encrypted => 'Windows stored this file encrypted (EFS); it cannot be recovered.',
    FileFlag.placeholder => 'This is a cloud or deduplication placeholder; its content was not on this disk.',
    FileFlag.sparse => 'The file contains empty regions that were never written.',
    FileFlag.noData => 'The file record has no content.',
    FileFlag.badSectors => 'Part of the file lies on unreadable sectors.',
    FileFlag.olderGeneration => 'It belongs to an older file system; its space may have been reused since.',
    FileFlag.embedded => 'It lies inside a disk image file. Recover that file and open it as a source.',
    FileFlag.stray => 'Its record belongs to no folder tree; where it came from is uncertain.',
    FileFlag.signatureOk => 'The beginning of the file matches its type.',
    FileFlag.signatureMismatch => 'The beginning of the file does not match its type; it was probably overwritten.',
    FileFlag.unknown => 'A condition this version of HDSavior does not know.',
  };

  static String flagName(FileFlag flag, String wire) => switch (flag) {
    FileFlag.deleted => 'Deleted',
    FileFlag.tornRecord => 'Torn record',
    FileFlag.missingFragments => 'Missing parts',
    FileFlag.sizeEstimated => 'Size estimated',
    FileFlag.outOfBounds => 'Outside scanned area',
    FileFlag.overwritten => 'Overwritten',
    FileFlag.overlap => 'Shared space',
    FileFlag.compressed => 'Compressed',
    FileFlag.encrypted => 'Encrypted',
    FileFlag.placeholder => 'Placeholder',
    FileFlag.sparse => 'Sparse',
    FileFlag.noData => 'No content',
    FileFlag.badSectors => 'Bad sectors',
    FileFlag.olderGeneration => 'Older file system',
    FileFlag.embedded => 'Inside an image file',
    FileFlag.stray => 'Stray record',
    FileFlag.signatureOk => 'Type confirmed',
    FileFlag.signatureMismatch => 'Type mismatch',
    FileFlag.unknown => wire,
  };

  // --- destinations ----------------------------------------------------------------

  static String verdictHeadline(DestinationVerdict verdict, ProtectionReason? reason) => switch (verdict) {
    DestinationVerdict.ok => 'This location can be used',
    DestinationVerdict.overlapsSource => switch (reason) {
      ProtectionReason.lostVolume => 'This location is inside the lost file system',
      ProtectionReason.sourceImage => 'This is the disk image being rescued',
      _ => 'This location is on the part of the disk being rescued',
    },
    DestinationVerdict.sameDisk => 'This location is on the disk you are rescuing',
    DestinationVerdict.unknownDisk => 'HDSavior cannot tell which disk this location is on',
    DestinationVerdict.notFound => 'This location does not exist',
    DestinationVerdict.notWritable => 'This location cannot be written to',
    DestinationVerdict.invalidPath => 'This is not a complete path',
    DestinationVerdict.unknown => 'This location was not accepted',
  };

  static String? verdictAdvice(DestinationVerdict verdict, ProtectionReason? reason) => switch (verdict) {
    DestinationVerdict.overlapsSource => switch (reason) {
      ProtectionReason.lostVolume =>
        'The file system that was lost reaches into this drive. Writing there would destroy files you want back. '
            'Choose a folder on another disk.',
      ProtectionReason.sourceImage => 'HDSavior never writes to its source. Choose another file.',
      _ => 'Writing there would destroy the files you want back. Choose a folder on another disk.',
    },
    DestinationVerdict.sameDisk =>
      'It is outside the protected area, but writing to the disk you rescue is risky. '
          'Use another disk if you can.',
    DestinationVerdict.unknownDisk =>
      'If it is on the disk you rescue, writing there would destroy the files you want back.',
    DestinationVerdict.notFound => 'Check the path, or create the folder first.',
    DestinationVerdict.notWritable => 'Choose another folder, or check its permissions.',
    DestinationVerdict.invalidPath =>
      'Type a full path that starts with a drive letter (D:\\Rescued) or a network share (\\\\server\\share\\Rescued).',
    _ => null,
  };

  static String lostFilesHere(String files, String bytes) =>
      '$files of the lost files ($bytes) are stored where this drive is.';
  static String freeSpace(String free, String? fs) => fs == null || fs.isEmpty ? '$free free' : '$free free · $fs';
  static String notEnoughSpace(String need, String free) => 'Not enough free space: $need needed, $free free.';
  static String fileSizeLimit(String fs, String limit) =>
      '$fs cannot hold files larger than $limit. Larger files will be skipped. '
      'An NTFS or exFAT drive does not have this limit.';
  static const String checkingDestination = 'Checking this location…';
  static const String destinationCheckFailed = 'This location could not be checked.';
  static const String consentGiven = 'You allowed this location.';
  static const String consentWithdraw = 'Withdraw';
  static const String useAnyway = 'Use it anyway…';
  static const String uncPathHint =
      'Network shares can be typed as \\\\server\\share\\folder. Drive letters that are mapped to '
      'network shares are not visible to HDSavior, because it runs as administrator.';

  // The two consents of ARCHITECTURE S3. Separate dialogs, separate wording.
  static const String sameDiskConsentTitle = 'Write to the disk you are rescuing?';
  static String sameDiskConsentBody({required bool hasCatalog}) =>
      'This location is on the same physical disk as the files you are rescuing. It lies outside the '
      'area HDSavior protects, but writing there is still risky:\n\n'
      '• Lost files that the scan has not found, or could not reach, may be stored there and would be '
      'overwritten.\n'
      '${hasCatalog ? '' : '• No scan has finished yet, so nobody knows how far the lost file system reached.\n'}'
      '• If the disk is failing, writing to it adds stress.\n\n'
      'A second disk, for example an external USB drive, is much safer.';
  static const String sameDiskConsentCheck = 'I understand that this can overwrite lost files.';
  static const String sameDiskConsentConfirm = 'Write there anyway';
  static const String unknownDiskConsentTitle = 'Use a location on an unknown disk?';
  static const String unknownDiskConsentBody =
      'HDSavior cannot tell which physical disk this location is on. This happens with network shares that '
      'point back to this computer, virtual drives and pooled drives (Storage Spaces).\n\n'
      'If this location is on the disk you are rescuing, writing there will destroy the files you want back. '
      'HDSavior cannot protect you here.';
  static const String unknownDiskConsentCheck = 'I am sure this location is on another disk.';
  static const String unknownDiskConsentConfirm = 'Use this location';
  static const String chooseAnother = 'Choose another location';

  // --- source screen ----------------------------------------------------------

  static const String sourceTitle = 'Choose what to rescue';
  static const String sourcePurpose =
      'Pick the disk that was formatted by mistake. HDSavior opens it for reading only.';
  static const String openImage = 'Open disk image…';
  static const String openSession = 'Open saved scan…';
  static const String imageFilesLabel = 'Disk images';
  static const String sessionFilesLabel = 'HDSavior scans';
  static const String openImageFailed = 'The disk image could not be opened';
  static const String openDiskFailed = 'The disk could not be opened';
  static const String openSessionFailed = 'The saved scan could not be opened';
  static const String lastSessionTitle = 'Continue with your last scan';
  static const String lastSessionOpen = 'Open it';
  static const String notElevatedTitle = 'HDSavior is not running as administrator';
  static const String notElevatedBody =
      'Windows only lets administrators read a disk directly. Close HDSavior, then start it again and '
      'answer "Yes" when Windows asks for permission.';
  static const String disksHeading = 'Disks on this computer';
  static const String refreshDisks = 'Look for disks again';
  static const String disksFailed = 'The disks could not be listed';
  static const String diskListFooter =
      'Connected the disk just now? Choose "Look for disks again". Disks that hold Windows, SSDs and '
      'pooled drives are shown too, but are rarely worth rescuing this way.';
  static const String noDisksTitle = 'No disks are visible';
  static const String noDisksText =
      'Connect the disk you want to rescue and choose "Look for disks again". If it is connected but not '
      'shown, check its cable and power supply.';
  static const String noDisksNotElevated =
      'HDSavior needs administrator rights to see disks. Start it again and allow it to make changes when '
      'Windows asks. (HDSavior still never writes to the disk you rescue.)';
  static const String noDisksNotWindows = 'Disks can only be listed on Windows. You can still open a disk image file.';

  static String diskTitle(String? model, int number) =>
      model == null || model.trim().isEmpty ? 'Disk $number' : model.trim();
  static String diskSubtitle(int number, String size, String bus, {String? media}) =>
      ['Disk $number', size, if (bus.isNotEmpty) bus, ?media].join(' · ');
  static String? media(MediaKind media) => switch (media) {
    MediaKind.hdd => 'Hard disk',
    MediaKind.ssd => 'SSD',
    MediaKind.unknown => null,
  };
  static String diskSource(String? model, int? number, String size) =>
      '${model == null || model.trim().isEmpty ? 'Disk' : model.trim()} (${number == null ? '' : 'disk $number, '}$size)';
  static String imageSource(String path, String size) => 'Disk image ${path.split(RegExp(r'[\\/]')).last} ($size)';
  static String fileSystem(FileSystemKind? fs) => switch (fs) {
    FileSystemKind.ntfs => 'NTFS',
    FileSystemKind.fat => 'FAT',
    FileSystemKind.exfat => 'exFAT',
    FileSystemKind.refs => 'ReFS',
    FileSystemKind.bitlocker => 'BitLocker',
    FileSystemKind.other => 'other file system',
    FileSystemKind.unknown => 'unknown file system',
    null => 'no file system',
  };
  static String volumeLine(String? label, String fs, String size) =>
      [if (label != null && label.isNotEmpty) label, fs, size].join(' · ');
  static String sectorSize(int logical, int physical) =>
      logical == physical ? '$logical-byte sectors' : '$logical-byte sectors ($physical physical)';
  static String sessionFileLine(String path) => 'Scan saved to $path';

  static const String badgeSystem = 'Windows runs from it';
  static const String badgeSystemTip =
      'Windows writes to this disk constantly. Rescuing files from it while Windows runs is risky.';
  static const String badgeTrim = 'Erases on format';
  static const String badgeSsd = 'SSD';
  static const String badgeTrimTip = 'This device supports TRIM: a format usually discards all data on it for good.';
  static const String badgeRemovable = 'Removable';
  static const String badgeVirtual = 'Virtual disk';
  static const String badgeSpaces = 'Storage Spaces';
  static const String badgeDynamic = 'Dynamic disk';
  static const String badgeUnsupportedTip =
      'HDSavior can scan it, but cannot tell what was written since the format on such a disk.';
  static const String badgeNoMedia = 'No medium';
  static const String noVolumes = 'No volume that Windows can see.';
  static const String chooseDisk = 'Choose this disk';

  static const String chooseOtherSource = 'Choose another';
  static const String browseOnlyTitle = 'The disk of this scan is not connected';
  static const String browseOnlyBody =
      'You can browse the results, but to preview or recover files, connect the disk and attach it.';
  static const String attachAction = 'Attach the disk…';
  static const String resultsReadyTitle = 'Scan results are ready';
  static String resultsReadyBody(String files) => '$files files were found.';
  static const String showResults = 'Show the results';
  static const String imageInsteadTitle = 'Make a copy of the whole disk first';
  static const String imageInsteadBody =
      'Recommended if the disk makes noises or reports read errors: copy it once to an image file on another '
      'disk, then rescue from the copy.';
  static const String createImage = 'Create disk image…';
  static const String closeSourceTitle = 'Choose another source?';
  static String closeSourceBody(String path) =>
      'The current results are closed. They are saved in $path and can be opened again later.';
  static const String closeSourceBodyUnsaved =
      'The current results are closed. This scan was not saved, so it would have to be repeated.';
  static const String closeSourceConfirm = 'Close the results';
  static const String scanChoiceTitle = 'What should be scanned?';
  static const String scanChoiceHint =
      'Scan the partition that was formatted. If the disk was also re-partitioned, or you are not sure, '
      'scan the whole disk: it takes longer but finds everything.';
  static String partitionTitle(int index, List<String> letters, String? label) => [
    index == 0 ? 'Whole volume' : 'Partition $index',
    if (letters.isNotEmpty) letters.join(' '),
    if (label != null && label.isNotEmpty) label,
  ].join(' · ');
  static String partitionSubtitle(String fs, String size, String type, String offset) =>
      [fs, size, if (type.isNotEmpty) type, 'starts at $offset'].join(' · ');
  static const String scanPartition = 'Scan this partition';
  static const String noPartitions = 'No partition table was found.';
  static String scanWholeTitle(bool disk) => disk ? 'The whole disk' : 'The whole image';
  static String scanWholeSubtitle(String size) => '$size, including everything outside the partitions';
  static String scanWhole(bool disk) => disk ? 'Scan the whole disk' : 'Scan the whole image';

  // --- flows: scanning, sessions, attaching ------------------------------------

  static const String newScanTitle = 'Start a new scan?';
  static const String newScanBody =
      'The current results are replaced by the new scan. If they were saved, you can open them again later.';
  static const String newScanConfirm = 'Start the new scan';
  static const String sessionLocationTitle = 'Where should the scan be saved?';
  static const String sessionLocationIntro =
      'HDSavior saves the progress of the scan to a file, so that a stopped or interrupted scan can '
      'continue, and the results can be opened again without scanning again.';
  static String sessionLocationRefused(String why) => 'The usual place cannot be used: $why.';
  static const String sessionFolderLabel = 'Folder for the scan file';
  static const String sessionFolderHint = r'For example D:\HDSavior';
  static const String sessionLocationConfirm = 'Save there and scan';
  static const String scanWithoutSaving = 'Scan without saving';
  static const String scanWithoutSavingTitle = 'Scan without saving?';
  static const String scanWithoutSavingBody =
      'If the scan is stopped, or the disk or the computer fails, it cannot continue and has to start over. '
      'The results are lost when HDSavior closes.';
  static const String scanWithoutSavingConfirm = 'Scan without saving';
  static const String openSessionReplaceTitle = 'Open another scan?';
  static const String openSessionReplaceBody =
      'The current results are closed. If they were saved, you can open them again later.';
  static const String openSessionReplaceConfirm = 'Open another scan';
  static String sessionSaved(String path) => 'The scan was saved to $path.';
  static const String saveSessionFailed = 'The scan could not be saved there';
  static const String attachTitle = 'Attach the disk';
  static String attachIntro(String recorded) =>
      'This scan was made of $recorded. Connect that disk, then choose it below. HDSavior checks that it '
      'is the same disk before it uses it.';
  static const String attachIntroUnknown = 'Choose the disk that was scanned.';
  static const String attachNoDisks = 'No disks are visible.';
  static const String attachForce = 'Attach anyway';
  static const String attachImage = 'Disk image file…';
  static const String attachConfirm = 'Attach';

  // --- scan screen ---------------------------------------------------------------

  static const String scanTitle = 'Scan';
  static const String scanPurpose =
      'HDSavior reads the disk once from start to end and collects what is left of the old files.';
  static const String scanNoSourceTitle = 'Nothing to scan yet';
  static const String scanNoSourceText = 'First choose the disk that was formatted by mistake.';
  static const String chooseSource = 'Choose a disk';
  static const String chooseWhatToScan = 'Choose what to scan';
  static const String scanReadyTitle = 'Ready to scan';
  static const String scanReadyText =
      'The disk is open. Choose on the Source screen whether to scan one partition or the whole disk.';
  static const String scanPausedTitle = 'This scan was stopped before the end';
  static const String scanPausedText =
      'The results show what was found so far. Continue the scan to read the rest of the disk.';
  static const String resumeScan = 'Continue the scan';
  static const String resumeFailed = 'The scan could not continue';
  static const String scanHasResultsTitle = 'The scan is complete';
  static const String scanHasResultsText = 'Look at the results and choose the files to recover.';
  static const String scanStarting = 'Starting the scan…';
  static const String scanStopping = 'Stopping… the files found so far are being prepared.';
  static String phaseTitle(ScanPhase? phase) => switch (phase) {
    ScanPhase.mftProbe => 'Looking for the old file table',
    ScanPhase.sweep => 'Reading the whole disk',
    ScanPhase.analyzing => 'Rebuilding the folders',
    _ => 'Scanning',
  };
  static String phaseStep(ScanPhase phase) => switch (phase) {
    ScanPhase.mftProbe => 'Find the file table',
    ScanPhase.sweep => 'Read the whole disk',
    ScanPhase.analyzing => 'Rebuild the folders',
    ScanPhase.unknown => 'Working',
  };
  static const String analyzingText =
      'Almost done: HDSavior is putting the folders back together. This takes a moment on a large disk.';
  static const String scanRead = 'Read';
  static String ofTotal(String done, String total) => '$done of $total';
  static const String speed = 'Speed';
  static const String timeLeft = 'Time left';
  static const String timeUnknown = 'estimating…';
  static const String elapsed = 'Elapsed';
  static const String scanPosition = 'Position';
  static const String scanPositionTip = 'How far into the disk HDSavior is reading right now.';
  static const String foundSoFar = 'Found so far';
  static const String foundFiles = 'Files';
  static const String foundFolders = 'Folders';
  static const String foundRecords = 'File records';
  static const String foundRecordsTip = 'Entries of the old file table. Each describes a file or a folder.';
  static const String foundIndexBlocks = 'Folder index blocks';
  static const String foundIndexTip = 'Pieces of old folder listings. They help to name folders whose entry was lost.';
  static const String foundSignatures = 'File signatures';
  static const String foundSignaturesTip =
      'Places where a known file type (photo, document, …) starts. Used later by "Find more files by signature".';
  static const String unreadable = 'Unreadable';
  static const String unreadableTip = 'Sectors the disk could not read. They are read as zeros.';
  static String badSectorsTitle(String bytes) => '$bytes could not be read so far';
  static const String badSectorsBody =
      'The disk has unreadable sectors. HDSavior skips them, and files that lie there are marked as damaged. '
      'If this number grows quickly the disk may be failing: stop the scan and make a disk image first.';
  static const String sweepHintTitle = 'Most of the folders are usually found by now';
  static const String sweepHintBody =
      'The old file table has been read. If you are in a hurry, you can stop here and look at the results; '
      'the rest of the disk can still be scanned later.';
  static const String stopExplanation =
      'Stopping keeps everything found so far. You can look at those files and recover them now, and '
      'continue the scan later.';
  static const String stopNotPossible = 'The folders are being rebuilt. This last step cannot be stopped.';
  static const String scanNotSaved = 'This scan is not being saved.';
  static String scanSavedTo(String path) => 'Progress is saved to $path';
  static const String stopAndShow = 'Stop and show results';
  static const String stopping = 'Stopping…';
  static const String sourceLostTitle = 'The disk stopped responding';
  static const String sourceLostBody =
      'Nothing is lost: everything read so far is saved. Check the cable and the power supply of the disk, '
      'connect it again, then continue the scan where it stopped.';
  static const String sourceLostBodyUnsaved =
      'Check the cable and the power supply of the disk and connect it again. As long as HDSavior stays open, '
      'the scan can continue where it stopped.';
  static const String reattachAndResume = 'Reconnect and continue';
  static const String scanFailedTitle = 'The scan stopped with an error';
  static const String scanStoppedTitle = 'The scan was stopped';
  static const String scanCompletedTitle = 'The scan is complete';

  // --- results: names of things ------------------------------------------------

  static const String unnamed = '(no name)';

  /// The display name of a node.
  static String nodeName(NodeRow row) => row.name.isEmpty ? unnamed : row.name;

  /// What a file system found by the scan is called in the tree.
  static String instanceName(Instance instance) {
    final label = instance.label.trim();
    return switch (instance.kind) {
      InstanceKind.main => 'Lost file system',
      InstanceKind.older => label.isEmpty ? '[Older file system]' : label,
      InstanceKind.other => '${label.isEmpty ? '[Another volume]' : label} · another volume',
      InstanceKind.embedded => '${label.isEmpty ? '[Inside an image file]' : label} · browse only',
      InstanceKind.strays => label.isEmpty ? '[Stray records]' : label,
      InstanceKind.unknown => label.isEmpty ? '[File system]' : label,
    };
  }

  /// What a file system is, in a sentence or two.
  static String instanceDescription(Instance instance) => switch (instance.kind) {
    InstanceKind.main =>
      'The file system that was formatted. Your files are most likely here, in their original folders.',
    InstanceKind.older =>
      'A file system from before an earlier format of the same disk. Much of its space has been reused since, '
          'so its files are at best uncertain.',
    InstanceKind.other => 'Another volume found on the disk, separate from the one that was formatted.',
    InstanceKind.embedded =>
      'A file system inside the disk image file ${instance.embeddedIn ?? ''}. Its files cannot be recovered '
          'one by one: recover the image file, then open it in HDSavior as a disk image.',
    InstanceKind.strays =>
      'File records that belong to no folder tree, for example copies of old file tables. Their files are '
          'grouped by the folder they once named, and are uncertain.',
    InstanceKind.unknown => 'A file system found by the scan.',
  };

  static String instanceTooltip(Instance instance, String files, String bytes) =>
      '${instanceDescription(instance)}\n$files files, $bytes';
  static String folderTooltip(String name, String files, String size, bool deleted) =>
      '$name\n$files files, $size${deleted ? '\nDeleted before the format' : ''}';

  static String nodeKind(NodeKind kind, bool deleted) {
    final what = switch (kind) {
      NodeKind.dir => 'Folder',
      NodeKind.file => 'File',
      NodeKind.lostDir => 'Folder whose name was lost',
      NodeKind.virtual => 'Folder made by HDSavior',
      NodeKind.carved => 'File found by its signature',
      NodeKind.unknown => 'Item',
    };
    return deleted ? '$what, deleted before the format' : what;
  }

  static String typeGroup(TypeGroup group) => switch (group) {
    TypeGroup.pictures => 'Pictures',
    TypeGroup.documents => 'Documents',
    TypeGroup.music => 'Music',
    TypeGroup.videos => 'Videos',
    TypeGroup.archives => 'Archives',
  };

  // --- results screen ---------------------------------------------------------------

  static const String resultsScanningTitle = 'The scan is still running';
  static const String resultsScanningText = 'The results appear when the scan ends, or when you stop it.';
  static const String showScan = 'Show the scan';
  static const String noCatalogTitle = 'No results yet';
  static const String noCatalogText = 'Scan a disk, or open a saved scan, to see the files that were found.';
  static const String upOneLevel = 'Up one level';
  static const String searching = 'Searching…';
  static String searchResults(String count, int n) => n == 1 ? '1 file found in' : '$count files found in';
  static String items(String count, int n) => n == 1 ? '1 item' : '$count items';
  static const String detailsShow = 'Show details';
  static const String detailsHide = 'Hide details';
  static const String searchHint = 'Search by name in this folder and below';
  static const String searchClear = 'Clear the search';
  static const String filterDeleted = 'Deleted files';
  static const String filterDeletedTip =
      'Also show files that had already been deleted before the format. Ticked deleted files are recovered too.';
  static String conditionFilter(int index) =>
      const ['All conditions', 'Hide damaged files', 'Good and uncertain only', 'Good only'][index];
  static String conditionLabel(String value) => value;
  static const String conditionTip = 'Show files by their condition';
  static String typeLabel(List<String> groups) =>
      groups.isEmpty ? 'All types' : (groups.length <= 2 ? groups.join(', ') : '${groups.length} types');
  static const String typeTip = 'Show only some kinds of files';
  static const String geometryChipValidated = 'Disk layout verified';
  static const String geometryChipUnverified = 'Disk layout not verified';
  static const String geometryChipValidatedShort = 'Layout verified';
  static const String geometryChipUnverifiedShort = 'Layout unverified';
  static const String geometryChipTip =
      'How HDSavior maps the lost file system onto the disk. Click to see the evidence.';
  static const String listFailed = 'This list could not be loaded';
  static const String noMatchesTitle = 'No files match';
  static const String noMatchesText = 'Try another name, or remove some filters.';
  static const String emptyFolderTitle = 'Nothing here';
  static const String emptyFolderText = 'This folder holds no files that were found.';
  static const String emptyFolderFiltered = 'Nothing matches the filters. Remove some filters to see more.';
  static String sortBy(String column) => 'Sort by $column';
  static const String columnName = 'Name';
  static const String columnSize = 'Size';
  static const String columnModified = 'Modified';
  static const String columnStatus = 'Condition';
  static const String deletedTip = 'Deleted before the format';
  static const String incompleteTitleShort = 'The scan was stopped before the end';
  static String incompleteTitle(String scanned, String total) =>
      'The scan was stopped before the end ($scanned of $total read)';
  static String outOfRangeTitle(String files, String bytes) => '$files files ($bytes) lie outside the scanned area';
  static const String scanWholeDisk = 'Scan the whole disk';
  static const String saveSessionElsewhere = 'Save the scan elsewhere…';
  static const String badSectorsCatalog =
      'Files that lie on unreadable sectors are marked as damaged. The rest is not affected.';
  static String moreNotices(int n) => n == 1 ? 'Show 1 more note' : 'Show $n more notes';
  static const String noticesTitle = 'Notes about these results';
  static const String moreAbout = 'More';
  static const String dismiss = 'Dismiss';

  // --- carving ---------------------------------------------------------------

  static const String carveAction = 'Find more files by signature';
  static const String carveActionShort = 'Find more files';
  static const String carveTip = 'Looks for photos, documents and other known file types whose file record is gone.';
  static const String carveNoHits = 'The scan did not collect file signatures, so this search is not possible.';
  static const String carveNoSource = 'Attach the disk first.';
  static const String carveBusy = 'Wait for the running task to finish.';
  static const String carveFailed = 'The search by signature failed';
  static const String carveTitle = 'Find more files by signature';
  static const String carveBody =
      'Some files have lost their entry in the file table, so they have no name and no folder any more. '
      'HDSavior can still find many of them by their content, for example photos and documents that were '
      'stored in one piece.\n\nFiles found this way appear in the folder [Carved files], sorted by type, '
      'with made-up names. Files that were stored in pieces cannot be found this way.';
  static const String carveKinds = 'File types to look for';
  static const String carveStart = 'Start the search';
  static const String carveStarting = 'Starting the search by signature…';
  static String carveProgress(String done, String total, String found, String bytes, String? eta) =>
      'Searching by signature: $done of $total places checked · $found files found ($bytes)'
      '${eta == null ? '' : ' · $eta left'}';
  static const String carveStop = 'Stop';
  static String carveDone(String found, String bytes, {required bool cancelled}) =>
      '${cancelled ? 'Search stopped. ' : ''}$found files ($bytes) were found by their signature.';
  static const String carveShow = 'Show them';

  // --- details ---------------------------------------------------------------------

  static const String detailsTitle = 'Details';
  static String pathLine(String path) => path.isEmpty ? '/' : path;
  static const String detailsFacts = 'Facts';
  static const String detailsRecord = 'File record (for experts)';
  static const String factSize = 'Size';
  static String bytesExact(String count) => '$count bytes';
  static const String factFiles = 'Files';
  static const String factFolders = 'Folders';
  static const String factDeleted = 'Deleted files';
  static String deletedFilesValue(String count, String bytes) => '$count ($bytes)';
  static const String factModified = 'Modified';
  static const String factCreated = 'Created';
  static const String factAccessed = 'Accessed';
  static const String factKind = 'Kind';
  static const String factOtherNames = 'Other names';
  static const String factRecord = 'Record number';
  static const String factSequence = 'Sequence';
  static const String factRecordOffset = 'Record on disk at';
  static const String factExtensionRecords = 'Extension records';
  static const String factStorage = 'Content';
  static const String storageResident = 'inside the file record';
  static String storageRuns(String runs) => '$runs fragment(s)';
  static const String factRealSize = 'Real size';
  static const String factInitialized = 'Written size';
  static const String factAllocated = 'Allocated size';
  static const String factFirstCluster = 'First cluster';
  static const String factFirstOffset = 'Content starts at';
  static const String factLostDirs = 'Unnamed folders';
  static const String factRecords = 'File records';
  static const String factTimeRange = 'Changed between';
  static String timeRange(String from, String to) => '$from and $to';
  static const String factEmbeddedIn = 'Inside the file';
  static const String factClusterSize = 'Cluster size';
  static const String factVolumeOffset = 'Volume starts at';
  static const String factVerified = 'Verified';
  static const String factVolumeSize = 'Volume size';
  static const String previewTitle = 'Preview';
  static const String previewNeedsSource = 'Attach the disk to see a preview.';
  static const String previewUnsupported = 'This file cannot be read, so there is no preview.';
  static const String previewEmpty = 'The file is empty.';
  static const String previewLoad = 'Show preview';
  static const String previewFailed = 'The preview could not be read';
  static String previewPictureLabel(String name) => 'Preview of $name';
  static const String previewNotAPicture = 'This does not look like a picture of its type. The beginning of the file:';
  static const String previewHex = 'Beginning of the file (hexadecimal)';
  static const String previewText = 'Beginning of the file (text)';
  static const String previewMore = 'Only the beginning is shown.';
  static const String instanceManual = 'You chose how this file system is treated.';
  static const String geometryHeading = 'Disk layout';
  static const String geometryOpen = 'Evidence and settings…';
  static const String instanceChoiceHeading = 'Is this the right file system?';
  static const String instanceChoiceHint =
      'HDSavior decides on its own which file system is the one that was formatted. If its choice is wrong, '
      'you can overrule it.';
  static const String instanceMakeMain = 'This is the one I want';
  static const String instanceMakeOther = 'Treat as a separate volume';
  static const String instanceMakeAuto = 'Undo my choice';
  static String instanceConfirmTitle(InstanceOverride kind) => switch (kind) {
    InstanceOverride.main => 'Make this the main file system?',
    InstanceOverride.other => 'Treat this as a separate volume?',
    InstanceOverride.auto => 'Let HDSavior decide again?',
  };
  static String instanceConfirmBody(InstanceOverride kind) =>
      '${switch (kind) {
        InstanceOverride.main => 'Its files are then shown as the main tree, and the file system HDSavior had chosen becomes '
            'another volume.',
        InstanceOverride.other => 'Its files are then read with its own disk layout and can be recovered.',
        InstanceOverride.auto => 'HDSavior applies its own choice again.',
      }}\n\nThe results are rebuilt: what you ticked is cleared, and files found by signature are discarded. '
      'Nothing on the disk changes.';
  static const String instanceConfirm = 'Rebuild the results';

  // --- geometry -------------------------------------------------------------------

  static const String geometryTitle = 'Disk layout';
  static const String geometryIntro =
      'To read a file, HDSavior must know where the lost file system started on the disk and how large its '
      'clusters (allocation units) were. Both are worked out from evidence on the disk.';
  static const String geometryInstance = 'File system';
  static const String geometryValidated = 'The layout is verified';
  static const String geometryValidatedBody = 'Files read with it start with the content their type promises.';
  static const String geometryNotValidated = 'The layout could not be verified';
  static const String geometryNotValidatedBody =
      'There was too little evidence. Files are marked as uncertain. If you know better, choose another '
      'candidate below.';
  static const String geometryCandidates = 'Candidates';
  static const String geometryNoCandidates = 'No other candidates were found.';
  static const String geometryAdvanced = 'Set it by hand';
  static const String geometryAdvancedHint = 'For experts. A wrong layout makes every file unreadable.';
  static const String geometryManualWarning =
      'Only change this if you know the old layout, for example from a backup of the partition table.';
  static const String geometryOffsetLabel = 'Volume start (bytes)';
  static const String geometryOffsetHelp = 'A multiple of 512';
  static const String geometryApply = 'Apply';
  static const String geometryConfirmTitle = 'Change the disk layout?';
  static String geometryConfirmBody({required bool main}) =>
      'The condition of every file of this file system is worked out again. What you ticked stays ticked.'
      '${main ? ' Files found by signature are discarded.' : ''} Nothing on the disk changes.';
  static const String geometryConfirm = 'Change the layout';
  static const String geometryFailed = 'The layout could not be changed';
  static const String offsetNotANumber = 'Enter a number of bytes.';
  static const String offsetNotAligned = 'Must be a multiple of 512.';
  static const String offsetOutside = 'This is beyond the end of the disk.';
  static String clusterSource(ClusterSource source) => switch (source) {
    ClusterSource.attr => 'from the file records',
    ClusterSource.boot => 'from a boot sector',
    ClusterSource.byDefault => 'assumed (no evidence)',
    ClusterSource.manual => 'set by you',
    ClusterSource.unknown => 'from an unknown source',
  };
  static String offsetSource(OffsetSource source) => switch (source) {
    OffsetSource.mft => 'from the map of the file table',
    OffsetSource.indexVotes => 'from folder listings',
    OffsetSource.boot => 'from a boot sector',
    OffsetSource.backupBoot => 'from a backup boot sector',
    OffsetSource.signature => 'from file signatures',
    OffsetSource.byDefault => 'partition start (no other evidence)',
    OffsetSource.inherited => 'taken from the main file system',
    OffsetSource.manual => 'set by you',
    OffsetSource.unknown => 'from an unknown source',
  };
  static const String evidenceDirs = 'Folder listings agreeing';
  static String evidenceDirsValue(String dirs, String blocks) => '$dirs folders ($blocks index blocks)';
  static const String evidenceSignatures = 'Files checked by content';
  static const String evidenceNoSignatures = 'none';
  static String evidenceSignaturesValue(String ok, String checked) => '$ok of $checked matched their type';
  static String candidateTitle(String cluster, String offset) => '$cluster clusters, volume at $offset';
  static String candidateEvidence(String source, String dirs, String ok, String checked) =>
      '$source · $dirs folders agree · $ok of $checked files matched';
  static const String candidateValidated = 'Verified';
  static const String candidateInUse = 'In use';
  static const String candidateUse = 'Use this';

  // --- selection bar ---------------------------------------------------------------

  static const String selectionEmpty =
      'Tick the files and folders you want to recover. Ticking a folder includes everything in it.';
  static const String selectionFailed = 'What you ticked could not be counted.';
  static const String selectionCounting = 'Counting what you ticked…';
  static String selectionSummary(String count, int n, String bytes) => '${n == 1 ? '1 file' : '$count files'}, $bytes';
  static String countDamaged(String n) => '$n damaged';
  static String countUncertain(String n) => '$n uncertain';
  static String countUnsupported(String n) => "$n can't be recovered";
  static const String selectionClear = 'Clear';
  static const String recoverAction = 'Recover…';

  // --- recover screen ----------------------------------------------------------------

  static const String recoverTitle = 'Recover';
  static const String recoverPurpose =
      'Copy the files you ticked to another disk. Nothing is written to the disk you are rescuing.';
  static const String recoverNoCatalogTitle = 'Nothing to recover yet';
  static const String recoverNoCatalog = 'Scan a disk first, then tick the files you want back.';
  static const String recoverWhat = '1. What will be recovered';
  static const String recoverChange = 'Change what is ticked';
  static const String recoverNothingTicked = 'Nothing is ticked yet';
  static const String recoverNothingTickedBody = 'On the Results screen, tick the files and folders you want back.';
  static const String recoverFiles = 'Files';
  static const String recoverBytes = 'Size';
  static const String recoverBytesTip = 'The size of the files that will be written.';
  static const String recoverFolders = 'Folders';
  static const String optionDeleted = 'Include files that were deleted before the format';
  static const String optionDeletedHint =
      'They are often incomplete. Leave this off unless you are looking for something you had deleted.';
  static const String optionSkipDamaged = 'Skip damaged files';
  static const String optionSkipDamagedHint =
      'Damaged files are partly overwritten or missing. A damaged photo or document is sometimes still '
      'partly usable, so they are recovered unless you switch this on.';
  static const String optionOnExists = 'If a file is already in the destination folder';
  static String onExists(OnExists value) => switch (value) {
    OnExists.rename => 'Keep both',
    OnExists.skip => 'Keep the old one',
    OnExists.overwrite => 'Replace the old one',
  };
  static const String optionOnExistsHint =
      '"Keep both" saves the new file as, for example, "photo (2).jpg". Nothing that was there before is '
      'replaced unless you choose "Replace the old one".';
  static const String recoverWhere = '2. Where to save the files';
  static const String recoverWhereHint =
      'Choose a folder on another disk, for example an external USB drive with enough free space.';
  static const String recoverFolderLabel = 'Destination folder';
  static const String recoverFolderHint = r'For example F:\Rescued';
  static const String recoverReady = 'Everything is ready.';
  static const String recoverStartPlain = 'Recover';
  static String recoverStart(String count, int n) => n == 1 ? 'Recover 1 file' : 'Recover $count files';
  static const String recoverNeedsSource =
      'To recover files, the disk that was scanned must be connected and attached.';
  static const String recoverWhyBusy = 'Wait for the running task to finish.';
  static const String recoverWhyNothing = 'Tick some files first.';
  static const String recoverWhyNoFiles = 'What is ticked contains no files with the current options.';
  static const String recoverWhyNoDestination = 'Choose a destination folder.';
  static const String recoverWhyChecking = 'Checking the destination…';
  static const String recoverWhyConsent = 'This destination needs your permission first (see above).';
  static const String recoverWhyRefused = 'This destination cannot be used. Choose another one.';
  static const String recoverWhySpace = 'There is not enough free space at the destination.';
  static const String recoverFailedToStart = 'The recovery could not start';
  static const String recoverStarting = 'Starting…';
  static const String recoverRunning = 'Recovering files…';
  static const String recoverStopping = 'Stopping after the current file…';
  static String recoverCurrent(String path) => 'Now: $path';
  static String recoverPlanned(String files, String bytes) => '$files files, $bytes';
  static const String recoverOutcomes = 'So far';
  static String outcomeOk(String n) => '$n OK';
  static String outcomeWarnings(String n) => '$n with warnings';
  static String outcomeFailed(String n) => '$n failed';
  static String outcomeSkipped(String n) => '$n skipped';
  static String recoverDestinationLine(String path) => 'Saving to $path';
  static const String recoverCancel = 'Stop';
  static const String recoverCancelTitle = 'Stop recovering?';
  static const String recoverCancelBody =
      'The files copied so far stay in the destination folder, together with a report. You can start again '
      'later; with "Keep the old one" the files already copied are not copied twice.';
  static const String recoverCancelConfirm = 'Stop';
  static const String recoverContinue = 'Keep going';
  static const String recoverCompleted = 'The recovery is finished';
  static const String recoverCompletedBody =
      'Open the folder and check a few files, especially those marked with warnings.';
  static const String recoverCancelled = 'The recovery was stopped';
  static const String recoverCancelledBody = 'The files copied until then are in the destination folder.';
  static const String recoverFailed = 'The recovery stopped with an error';
  static const String recoverSummary = 'Result';
  static const String outcomeOkLabel = 'OK';
  static const String outcomeOkTip = 'Written, and everything indicates the file is intact.';
  static const String outcomeWarningsLabel = 'With warnings';
  static const String outcomeWarningsTip = 'Written, but the file may be damaged or incomplete. Check it.';
  static const String outcomeFailedLabel = 'Failed';
  static const String outcomeFailedTip = 'Could not be written. Nothing was left at the destination for it.';
  static const String outcomeSkippedLabel = 'Skipped';
  static const String outcomeSkippedTip =
      'Not written on purpose: not recoverable, damaged with "Skip damaged files", already there, or too large.';
  static const String outcomeNotAttempted = 'Not reached';
  static const String recoverWritten = 'Written';
  static const String recoverReportAdvice =
      'The report lists every file with its condition and the reason for each warning, failure or skip. '
      'It opens in a spreadsheet program.';
  static const String recoverDestination = 'Folder';
  static const String recoverReport = 'Report';
  static const String openFolder = 'Open folder';
  static const String openReport = 'Open report';
  static String openFailed(String path) => 'No program could be started to open it. The path is: $path';
  static const String recoverMore = 'Recover more files';
  static const String recoverBackToForm = 'Back to the settings';

  // --- image screen -----------------------------------------------------------------

  static const String imageTitle = 'Disk image';
  static const String imagePurpose =
      'Copy the disk sector by sector into a file on another disk. A failing disk is then read only once, '
      'and the rescue can continue from the copy.';
  static const String imageNoSourceTitle = 'Choose a disk first';
  static const String imageNoSourceText = 'Choose the disk to copy on the Source screen.';
  static const String imageNeedsSource = 'To make an image, the disk must be connected and attached.';
  static const String imageWhat = 'What to copy';
  static String imageWhole(bool disk, String size) => disk ? 'The whole disk ($size)' : 'The whole image ($size)';
  static String imagePartition(String title, String size) => '$title ($size)';
  static String imageScanned(String size) => 'The scanned area ($size)';
  static const String imageWhere = 'Where to save the image';
  static const String imageWhereHint =
      'The image is as large as what is copied. Choose a file on another disk with enough free space.';
  static const String imageFileLabel = 'Image file';
  static const String imageFileHint = r'For example F:\Images\disk1.img';
  static String imageTooLarge(String fs, String limit) =>
      '$fs cannot hold files larger than $limit, so the image does not fit there. Use an NTFS or exFAT drive.';
  static const String imageResume = 'Continue an interrupted image';
  static const String imageResumeHint =
      'The image file already exists from an earlier, stopped copy of this same disk. HDSavior checks this '
      'before it continues.';
  static const String imageDuration = 'Copying a large disk takes hours. You can stop and continue later.';
  static const String imageStart = 'Create the image';
  static const String imageContinue = 'Continue the image';
  static const String imageExistsTitle = 'This file already exists';
  static const String imageExistsBody =
      'If it is an interrupted image of this same disk, HDSavior can continue it. Otherwise choose another '
      'file name: HDSavior never overwrites an existing image.';
  static const String imageExistsResume = 'Continue the image';
  static const String imageFailedToStart = 'The image could not be started';
  static const String imageStarting = 'Starting…';
  static const String imageRunning = 'Copying the disk…';
  static const String imageStopping = 'Stopping…';
  static const String imageCopied = 'Copied';
  static const String imageBadSectors =
      'Unreadable sectors are written as zeros and listed in a file next to the image. Files there are '
      'marked as damaged when you rescue from the image.';
  static String imageDestinationLine(String path) => 'Writing to $path';
  static const String imageCancel = 'Stop';
  static const String imageCancelTitle = 'Stop the image?';
  static const String imageCancelBody =
      'What was copied so far is kept. You can continue the image later with "Continue an interrupted image".';
  static const String imageCancelConfirm = 'Stop';
  static const String imageKeepGoing = 'Keep going';
  static const String imageCompleted = 'The image is finished';
  static const String imageCompletedBody =
      'You can now disconnect the disk and rescue your files from the image instead.';
  static String imageCompletedBad(String bytes) =>
      '$bytes could not be read and were written as zeros. You can still rescue from the image.';
  static const String imageCancelled = 'The image was stopped';
  static const String imageCancelledBody = 'It can be continued later.';
  static const String imageFailed = 'The image stopped with an error';
  static const String imageFile = 'Image file';
  static const String imageWritten = 'Written';
  static const String imageBadMap = 'Unreadable sectors listed in';
  static const String imageOpenAsSource = 'Rescue from this image';
  static const String imageOpenTitle = 'Rescue from the image?';
  static const String imageOpenConfirm = 'Open the image';
  static const String imageBackToResume = 'Back (to continue later)';
  static const String imageBackToForm = 'Back to the settings';
  static const String imageAnother = 'Make another image';
  static String imageCopyName(String file) => 'Copy of $file';
  static const String imageDefaultName = 'Disk';
}
