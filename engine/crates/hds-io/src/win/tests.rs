use std::path::Path;

use super::volumes::{
    computer_names, enumerate_volumes, final_path, free_space, open_for_query, page_file_letter,
    system_volume_device, volume_device_of, volume_info_by_handle,
};
use super::*;
use crate::test_util::TempDir;
use crate::winpath;

#[test]
fn wide_strings_round_trip_and_refuse_interior_nul() {
    assert_eq!(wide_units("aé"), vec![0x61, 0xE9]);
    assert_eq!(wide_nul("ab").unwrap(), vec![0x61, 0x62, 0]);
    assert!(wide_nul("a\0b").is_err());
    assert_eq!(from_wide(&[0x61, 0x62, 0, 0x63]), OsString::from("ab"));
    assert_eq!(string_from_wide(&[0x61, 0xD800, 0]), "a\u{FFFD}");
    assert_eq!(
        string_from_wide(&[0x61, 0x62]),
        "ab",
        "no terminator: the whole buffer"
    );
}

#[test]
fn full_path_normalises_without_touching_the_disk() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(full_path(Path::new("rel/../x.")).unwrap(), cwd.join("x"));
    assert_eq!(
        full_path(Path::new(r"C:\a\.\b\..\c")).unwrap(),
        Path::new(r"C:\a\c")
    );
    assert_eq!(
        full_path(Path::new("C:/a/b")).unwrap(),
        Path::new(r"C:\a\b")
    );
    assert_eq!(
        full_path(Path::new(r"\\server\share\x\..\y")).unwrap(),
        Path::new(r"\\server\share\y")
    );
    // A bare reserved name is a device, and is recognised as one afterwards.
    let nul = full_path(Path::new("NUL")).unwrap();
    assert!(
        matches!(
            winpath::classify(&wide_units(&nul)),
            winpath::WinPath::Invalid(_)
        ),
        "{nul:?}"
    );
    assert!(full_path(Path::new("a\0b")).is_err());
    assert!(full_path(Path::new("")).is_err());
}

#[test]
fn positional_reads_fill_the_buffer_or_fail() {
    let tmp = TempDir::new("win-read");
    let path = tmp.join("data.bin");
    let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let file = File::open(&path).unwrap();
    let mut buf = [0u8; 1000];
    read_exact_at(&file, 1500, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[1500..2500]);
    read_exact_at(&file, 2000, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[2000..]);
    // Across and at the end: an error, never a short count.
    let err = read_exact_at(&file, 2500, &mut buf).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(
        read_exact_at(&file, 3000, &mut buf).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    read_exact_at(&file, 3000, &mut []).unwrap();
}

#[test]
fn file_identity_is_the_same_under_every_name() {
    let tmp = TempDir::new("win-id");
    std::fs::write(tmp.join("a.bin"), b"a").unwrap();
    std::fs::write(tmp.join("b.bin"), b"b").unwrap();
    let id = |name: &str| file_id(&File::open(tmp.join(name)).unwrap()).unwrap();
    assert_eq!(id("a.bin"), id("A.BIN"));
    assert_ne!(id("a.bin"), id("b.bin"));
    assert_eq!(id("a.bin").volume, id("b.bin").volume);
    if std::fs::hard_link(tmp.join("a.bin"), tmp.join("hard.bin")).is_ok() {
        assert_eq!(id("a.bin"), id("hard.bin"));
    }
    let info = handle_info(&open_for_query(tmp.path()).unwrap()).unwrap();
    assert_ne!(
        info.dwFileAttributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
        0
    );
}

#[test]
fn a_directory_resolves_to_its_volume() {
    let tmp = TempDir::new("win-volume");
    let handle = open_for_query(tmp.path()).unwrap();
    let guid_path = final_path(&handle, true).unwrap();
    let units = wide_units(&guid_path);
    assert!(
        winpath::volume_device_prefix(&units).is_some(),
        "{guid_path:?}"
    );
    let device = volume_device_of(&handle).unwrap();
    assert!(
        winpath::is_volume_guid_device(&wide_units(&device)),
        "{device:?}"
    );
    let dos_path = final_path(&handle, false).unwrap();
    assert!(
        dos_path.to_string_lossy().starts_with(r"\\?\"),
        "{dos_path:?}"
    );
    // The volume the directory is on is one of the machine's volumes.
    let volumes = enumerate_volumes();
    assert!(volumes.iter().any(|v| v
        .device
        .to_string_lossy()
        .eq_ignore_ascii_case(&device.to_string_lossy())));
    for volume in &volumes {
        assert!(winpath::is_volume_guid_device(&wide_units(&volume.device)));
        assert!(volume.root().to_string_lossy().ends_with('\\'));
        // A volume with a drive letter is reachable; one with no mount path is
        // never asked through its root (that would mount it).
        assert_eq!(volume.is_reachable(), !volume.mount_paths.is_empty());
        if !volume.letters().is_empty() {
            assert!(volume.is_reachable());
        }
        for letter in volume.letters() {
            assert!(letter.len() == 2 && letter.ends_with(':'), "{letter}");
        }
    }
    let info = volume_info_by_handle(&handle).unwrap();
    assert!(!info.fs_name.is_empty());
    assert!(info.max_component >= 100);
    assert!(free_space(tmp.path()).unwrap() > 0);
    // Opening for a query creates nothing and fails for what does not exist.
    assert!(open_for_query(&tmp.join("missing")).is_err());
    assert!(!tmp.join("missing").exists());
}

#[test]
fn facts_about_this_machine_are_available() {
    let names = computer_names();
    assert!(!names.is_empty());
    assert!(names.iter().all(|n| !n.is_empty()));
    assert!(winpath::is_local_server(&names[0], &names));
    let system = system_volume_device().expect("the Windows directory is on a volume");
    assert!(
        winpath::is_volume_guid_device(&wide_units(&system)),
        "{system:?}"
    );
}

#[test]
fn page_file_entries_name_their_drive() {
    assert_eq!(page_file_letter(r"D:\pagefile.sys 0 0"), Some('D'));
    assert_eq!(page_file_letter(r"c:\pagefile.sys"), Some('C'));
    assert_eq!(page_file_letter(r"\??\E:\pagefile.sys"), Some('E'));
    assert_eq!(
        page_file_letter(r"?:\pagefile.sys"),
        None,
        "the system picks the drive"
    );
    assert_eq!(page_file_letter(""), None);
    assert_eq!(page_file_letter("pagefile.sys"), None);
    assert_eq!(page_file_letter(r"\??\Volume{x}\pagefile.sys"), None);
    // Reading the registry value does not fail, whatever it holds.
    let _ = super::volumes::page_file_letters();
}

#[test]
fn a_volume_without_mount_paths_is_not_reachable() {
    let volume = volumes::Volume {
        device: OsString::from(r"\\?\Volume{00000000-0000-0000-0000-000000000000}"),
        extents: None,
        device_number: None,
        mount_paths: Vec::new(),
    };
    assert!(!volume.is_reachable());
    let mounted = volumes::Volume {
        mount_paths: vec![r"E:\".to_string()],
        ..volume
    };
    assert!(mounted.is_reachable());
}
