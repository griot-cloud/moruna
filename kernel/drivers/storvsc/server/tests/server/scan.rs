//! Finding the disks and naming them.

use moruna_storvsc::{NtStatus, Operation, ScsiStatus, SrbStatus, Storvsc};
use moruna_storvsc_server::{
    Controller, Served,
    naming::{BOOT, DATA, Name, RESOURCE, ROOT, name},
    plan,
    scan::{self, Scan, no_device},
};
use vmbus_protocol::Guid;

use crate::{Page, at, controller, fake::Fault, plan_of, scanned};

/// An instance of no Azure controller.
const OTHER: Guid = Guid {
    data1: 0x1234_5678,
    data2: 0,
    data3: 0,
    data4: [0; 8],
};

/// A controller whose instance starts `00000000-<second>-`.
fn zeros(second: u16) -> Guid {
    Guid {
        data1: 0,
        data2: second,
        data3: 0x8899,
        data4: [0; 8],
    }
}

#[test]
fn report_luns_finds_each_disk_of_the_target_with_its_size() {
    let (link, host) = controller(&[(0, 512, 4096), (1, 4096, 100), (9, 512, 64)]);
    let (_, scan) = scanned(link, &host);
    let found: Vec<_> = scan
        .disks
        .iter()
        .map(|d| (d.address(), d.block_size(), d.blocks()))
        .collect();
    assert_eq!(
        found,
        [(at(0), 512, 4096), (at(1), 4096, 100), (at(9), 512, 64)]
    );
    assert!(scan.notes.is_empty());
    // One REPORT LUNS and a READ CAPACITY a disk, after the initialisation's four steps.
    let srbs = host
        .borrow()
        .seen
        .iter()
        .filter(|(op, _)| *op == Operation::EXECUTE_SRB)
        .count();
    assert_eq!(srbs, 4);
}

#[test]
fn a_controller_that_refuses_report_luns_has_luns_0_to_63_probed() {
    let (link, host) = controller(&[(0, 512, 64), (63, 512, 64)]);
    host.borrow_mut().refuse_report_luns = true;
    let (_, scan) = scanned(link, &host);
    let luns: Vec<_> = scan.disks.iter().map(|d| d.address().lun).collect();
    assert_eq!(luns, [0, 63]);
    assert_eq!(scan.notes.len(), 1);
    assert!(scan.notes[0].starts_with("REPORT LUNS was refused"));
    assert!(scan.notes[0].ends_with("LUNs 0 to 63 probed"));
}

#[test]
fn a_target_with_no_disk_has_none() {
    let (link, host) = controller(&[]);
    let (_, scan) = scanned(link, &host);
    assert_eq!(scan, Scan::default());
}

#[test]
fn a_lun_that_answers_and_has_no_capacity_is_noted_and_not_served() {
    let (link, host) = controller(&[(0, 512, 64), (2, 512, 64)]);
    {
        let mut host = host.borrow_mut();
        // REPORT LUNS and LUN 0 pass; LUN 2 is the DVD drive, with nothing in it.
        host.faults.push_back(Fault::Pass);
        host.faults.push_back(Fault::Pass);
        host.faults.push_back(Fault::Scsi {
            srb: SrbStatus::ERROR,
            scsi: ScsiStatus::CHECK_CONDITION,
            sense: None,
        });
    }
    let (_, scan) = scanned(link, &host);
    assert_eq!(scan.disks.len(), 1);
    assert_eq!(scan.notes.len(), 1);
    assert!(scan.notes[0].starts_with("lun 2: no capacity"));
}

#[test]
fn a_capacity_that_is_no_capacity_is_noted() {
    let (link, host) = controller(&[(0, 512, 64)]);
    {
        let mut host = host.borrow_mut();
        host.faults.push_back(Fault::Pass);
        // Fewer bytes than a block count and a block size.
        host.faults.push_back(Fault::Moved(4));
    }
    let (_, scan) = scanned(link, &host);
    assert!(scan.disks.is_empty());
    assert!(scan.notes[0].contains("its capacity is"));
}

#[test]
fn a_lun_list_that_is_no_list_has_luns_probed() {
    let (link, host) = controller(&[(5, 512, 64)]);
    // Fewer bytes than the list's header.
    host.borrow_mut().faults.push_back(Fault::Moved(3));
    let (_, scan) = scanned(link, &host);
    assert_eq!(scan.disks.len(), 1);
    assert!(scan.notes[0].starts_with("REPORT LUNS answered"));
}

#[test]
fn a_controller_that_refuses_its_initialisation_or_never_answers_ends_the_scan() {
    let (link, host) = controller(&[]);
    host.borrow_mut().refuse = Some((Operation::QUERY_PROPERTIES, NtStatus::INVALID_PARAMETER));
    let page = Page(host.clone());
    let refused = scan::start(link, &page).err().unwrap();
    assert!(refused.starts_with("the initialisation: the host refused"));

    let (link, host) = controller(&[]);
    host.borrow_mut().stalled = true;
    let page = Page(host.clone());
    let silent = scan::start(link, &page).err().unwrap();
    assert_eq!(silent, "no answer to the initialisation within 4 s");

    let (link, host) = controller(&[(0, 512, 64)]);
    let page = Page(host.clone());
    let mut device: Storvsc<_> = scan::start(link, &page).unwrap();
    host.borrow_mut().stalled = true;
    let late = scan::scan(&mut device, &page).err().unwrap();
    assert_eq!(late, "no answer to REPORT LUNS within 4 s");
    host.borrow_mut().rescind = true;
    let gone = scan::scan(&mut device, &page).err().unwrap();
    assert!(gone.starts_with("REPORT LUNS: "), "{gone}");
}

#[test]
fn no_device_is_the_statuses_of_an_address_with_nothing_there() {
    for srb in [
        SrbStatus::INVALID_LUN,
        SrbStatus::INVALID_TARGET_ID,
        SrbStatus::INVALID_PATH_ID,
        SrbStatus::NO_DEVICE,
        SrbStatus::NO_HBA,
        SrbStatus::SELECTION_TIMEOUT,
    ] {
        assert!(no_device(srb), "{srb:?}");
    }
    assert!(!no_device(SrbStatus::ERROR));
    assert!(!no_device(SrbStatus::BUSY));
}

#[test]
fn azure_names_the_os_and_resource_disks_by_the_root_controller_and_data_disks_by_lun() {
    let boot = Some(Name {
        serial: BOOT.into(),
        read_only: true,
        boot: true,
    });
    let resource = Some(Name {
        serial: RESOURCE.into(),
        read_only: false,
        boot: false,
    });
    assert_eq!(name(ROOT, 0), boot);
    assert_eq!(name(ROOT, 1), resource);
    assert_eq!(name(ROOT, 2), None, "the DVD drive");
    assert_eq!(name(ROOT, 3), None);
    for data in DATA {
        assert_eq!(
            name(data, 7),
            Some(Name {
                serial: "lun-7".into(),
                read_only: false,
                boot: false
            })
        );
    }
    assert_eq!(name(zeros(0), 0), boot);
    assert_eq!(name(zeros(1), 0), resource);
    assert_eq!(name(zeros(2), 0), None);
    assert_eq!(name(OTHER, 0), None);
    assert_eq!(ROOT.to_string(), "f8b3781a-1e82-4818-a1c3-63d806ec15bb");
    let data: Vec<String> = DATA.iter().map(Guid::to_string).collect();
    assert_eq!(
        data,
        [
            "f8b3781b-1e82-4818-a1c3-63d806ec15bb",
            "f8b3781c-1e82-4818-a1c3-63d806ec15bb",
            "f8b3781d-1e82-4818-a1c3-63d806ec15bb"
        ]
    );
}

#[test]
fn the_plan_serves_named_disks_and_says_each_with_its_place() {
    let (root, root_host) = controller(&[(0, 512, 4096), (1, 512, 64), (2, 512, 8)]);
    let (data, data_host) = controller(&[(0, 4096, 1000), (5, 512, 64)]);
    let (_, root_scan) = scanned(root, &root_host);
    let (_, data_scan) = scanned(data, &data_host);
    let plan = plan(&[
        Controller {
            instance: ROOT,
            scan: &root_scan,
        },
        Controller {
            instance: DATA[0],
            scan: &data_scan,
        },
    ]);
    let names: Vec<_> = plan
        .served
        .iter()
        .map(|s| (s.controller, s.name.serial.as_str(), s.name.read_only))
        .collect();
    assert_eq!(
        names,
        [
            (0, "boot", true),
            (0, "resource", false),
            (1, "lun-0", false),
            (1, "lun-5", false)
        ]
    );
    let root = ROOT.to_string();
    assert_eq!(
        plan.lines,
        [
            format!(
                "storvsc: controller {root} target 0 lun 2: 8 blocks of 512 bytes, no Azure name: not served"
            ),
            format!(
                "storvsc: controller {root} target 0 lun 0: 4096 blocks of 512 bytes, boot, read-only, the boot disk"
            ),
            format!("storvsc: controller {root} target 0 lun 1: 64 blocks of 512 bytes, resource"),
            format!(
                "storvsc: controller {} target 0 lun 0: 1000 blocks of 4096 bytes, lun-0",
                DATA[0]
            ),
            format!(
                "storvsc: controller {} target 0 lun 5: 64 blocks of 512 bytes, lun-5",
                DATA[0]
            ),
            "storvsc: scan done: 5 disks on 2 SCSI controllers, 4 served".to_string(),
        ]
    );
}

#[test]
fn with_no_os_disk_every_disk_is_served_read_only_and_the_console_says_why() {
    let (data, host) = controller(&[(0, 512, 64), (1, 512, 64)]);
    let (_, scan) = scanned(data, &host);
    let plan = plan_of(DATA[1], &scan);
    assert!(plan.served.iter().all(|s: &Served| s.name.read_only));
    assert!(plan.lines[0].starts_with("storvsc: no disk is Azure's OS disk"));
    assert!(plan.lines[0].ends_with("every disk is served read-only"));
    assert_eq!(plan.lines.len(), 4);
    // A controller Azure does not name serves nothing, and needs no fallback.
    let plan = plan_of(OTHER, &scan);
    assert!(plan.served.is_empty());
    assert_eq!(
        plan.lines.last().unwrap(),
        "storvsc: scan done: 2 disks on 1 SCSI controllers, 0 served"
    );
    // Notes come first.
    let noted = Scan {
        disks: Vec::new(),
        notes: vec!["REPORT LUNS was refused".into()],
    };
    let plan = plan_of(ROOT, &noted);
    assert_eq!(
        plan.lines[0],
        format!("storvsc: controller {ROOT}: REPORT LUNS was refused")
    );
}
