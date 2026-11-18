//! The images a job's start is handed (item 3.6): verified against their registered SHA-256, an
//! altered one refused, a second job served from the cache, and the least recently used evicted.

extern crate std;

use super::*;
use alloc::{collections::BTreeMap, vec};

/// A server of images in memory, which counts what it is asked for; what it serves may be
/// changed after registration, as an altered image would be.
#[derive(Default)]
struct Served {
    objects: BTreeMap<String, Vec<u8>>,
    asked: Vec<(String, u64)>,
    down: bool,
}

impl Served {
    fn with(objects: &[(&str, &[u8])]) -> Self {
        Self {
            objects: objects
                .iter()
                .map(|(address, bytes)| (address.to_string(), bytes.to_vec()))
                .collect(),
            ..Self::default()
        }
    }
}

impl Fetch for Served {
    fn fetch(&mut self, address: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
        self.asked.push((address.to_string(), limit));
        if self.down {
            return Err(FetchError::Failed(
                "the host refused the connection".to_string(),
            ));
        }
        match self.objects.get(address) {
            Some(bytes) if bytes.len() as u64 > limit => Err(FetchError::TooLarge),
            Some(bytes) => Ok(bytes.clone()),
            None => Err(FetchError::Failed(format!("{address}: 404"))),
        }
    }
}

/// The function `name` registered for `image` at `address`.
fn registered(name: &str, address: &str, image: &[u8]) -> Registered {
    let sha256 = hex(&sha256::digest(image));
    Registered::new(name, address, &sha256).expect("64 lowercase hex digits")
}

const SCORE: &[u8] = b"\x7fELF gate-score";
const ENRICH: &[u8] = b"\x7fELF gate-enrich, a little longer";

#[test]
fn an_image_is_fetched_verified_and_a_second_job_finds_it_kept() {
    let mut served = Served::with(&[("https://x/gate-score", SCORE)]);
    let mut cache = Cache::new(1 << 20);
    let score = registered("gate-score", "https://x/gate-score", SCORE);
    let image = cache.image(&score, &mut served).expect("a verified image");
    assert_eq!(&*image, SCORE);
    assert_eq!(
        served.asked,
        [("https://x/gate-score".to_string(), 1 << 20)],
        "limited to the cache"
    );
    assert!(cache.holds(&score.sha256) && cache.len() == 1 && !cache.is_empty());
    assert_eq!(cache.held(), SCORE.len() as u64);
    // The second job that uses it is not fetched for.
    let again = cache
        .images(core::slice::from_ref(&score), &mut served)
        .unwrap();
    assert_eq!(&*again[0], SCORE);
    assert_eq!(served.asked.len(), 1, "not fetched again");
    assert_eq!(cache.counts(), (1, 1));
    // The same image under another name is the same kept image.
    let alias = registered("score-v1", "https://x/elsewhere", SCORE);
    cache.image(&alias, &mut served).unwrap();
    assert_eq!((served.asked.len(), cache.len()), (1, 1));
    assert_eq!(cache.capacity(), 1 << 20);
}

#[test]
fn an_image_changed_after_registration_is_refused_and_never_kept() {
    let score = registered("gate-score", "https://x/gate-score", SCORE);
    let mut served = Served::with(&[("https://x/gate-score", b"\x7fELF gate-score, altered")]);
    let mut cache = Cache::new(1 << 20);
    let refused = cache.image(&score, &mut served).unwrap_err();
    let Refused::Altered {
        ref name,
        expected,
        found,
    } = refused
    else {
        panic!("not altered: {refused:?}");
    };
    assert_eq!((name.as_str(), expected), ("gate-score", score.sha256));
    assert_eq!(found, sha256::digest(b"\x7fELF gate-score, altered"));
    let sentence = refused.to_string();
    assert!(
        sentence
            .starts_with("the function `gate-score`'s image has changed since it was registered")
    );
    assert!(sentence.contains(&hex(&score.sha256)));
    assert!(cache.is_empty(), "an altered image is never kept");
    // A job whose second function is altered is refused, naming it.
    let enrich = registered("gate-enrich", "https://x/gate-enrich", ENRICH);
    served
        .objects
        .insert("https://x/gate-score".into(), SCORE.to_vec());
    served
        .objects
        .insert("https://x/gate-enrich".into(), b"tampered".to_vec());
    let refused = cache.images(&[score, enrich], &mut served).unwrap_err();
    assert!(refused.to_string().contains("`gate-enrich`"));
}

#[test]
fn the_least_recently_used_image_is_evicted_to_make_room() {
    // Room for two of these three images, not three.
    let images: [(&str, &[u8]); 3] = [
        ("https://x/a", &[1; 400]),
        ("https://x/b", &[2; 400]),
        ("https://x/c", &[3; 400]),
    ];
    let mut served = Served::with(&images);
    let mut cache = Cache::new(1000);
    let [a, b, c] = images.map(|(address, bytes)| registered(&address[10..], address, bytes));
    cache.image(&a, &mut served).unwrap();
    cache.image(&b, &mut served).unwrap();
    // `a` is used again, so `b` is now the least recently used.
    cache.image(&a, &mut served).unwrap();
    cache.image(&c, &mut served).unwrap();
    assert!(cache.holds(&a.sha256) && cache.holds(&c.sha256) && !cache.holds(&b.sha256));
    assert_eq!(cache.held(), 800);
    // `b` again is fetched again, and evicts `a`, now the least recently used.
    cache.image(&b, &mut served).unwrap();
    assert!(!cache.holds(&a.sha256) && cache.holds(&b.sha256) && cache.holds(&c.sha256));
    assert_eq!(cache.counts(), (4, 1));
    assert!(cache.held() <= cache.capacity());
}

#[test]
fn an_image_larger_than_the_cache_or_unreachable_is_refused() {
    let mut cache = Cache::new(16);
    let big = registered("big", "https://x/big", &[7; 17]);
    let mut served = Served::with(&[("https://x/big", &[7; 17])]);
    let refused = cache.image(&big, &mut served).unwrap_err();
    assert_eq!(
        refused,
        Refused::TooLarge {
            name: "big".into(),
            limit: 16
        }
    );
    assert!(
        refused
            .to_string()
            .contains("larger than the node's function cache of 16 bytes")
    );
    // A fetch that brings more than it was allowed is refused the same.
    struct Careless;
    impl Fetch for Careless {
        fn fetch(&mut self, _address: &str, _limit: u64) -> Result<Vec<u8>, FetchError> {
            Ok(vec![7; 17])
        }
    }
    assert!(matches!(
        cache.image(&big, &mut Careless),
        Err(Refused::TooLarge { .. })
    ));
    served.down = true;
    let small = registered("small", "https://x/small", b"tiny");
    let refused = cache.image(&small, &mut served).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the function `small`'s image could not be fetched: the host refused the connection"
    );
    assert!(cache.is_empty());
}

/// The cache's account, of `limit` bytes.
fn account(limit: u64) -> moruna_svc::Account {
    moruna_svc::Account::carve(limit, "functions").unwrap()
}

/// What a test reads of a granted memory object: its first `len` bytes.
fn read(handle: &moruna_svc::Handle, len: usize) -> Vec<u8> {
    let memory = moruna_svc::Memory(handle.duplicate(Rights::READ | Rights::MAP).unwrap());
    let view = memory
        .map(0, (len as u64).div_ceil(4096) * 4096, moruna_svc::map::READ)
        .unwrap();
    let mut out = vec![0; len];
    view.read(0, &mut out).unwrap();
    out
}

use alloc::rc::Rc;
use moruna_svc::Rights;

#[test]
fn on_a_node_an_image_is_kept_once_in_a_memory_object_of_the_caches_account() {
    let cache_account = account(64 << 10);
    let mut keep = |bytes: Vec<u8>| MemoryImage::new(&bytes, &cache_account.0).map(Rc::new);
    let mut served = Served::with(&[
        ("https://x/gate-score", SCORE),
        ("https://x/gate-enrich", ENRICH),
    ]);
    let mut cache: Cache<Rc<MemoryImage>> = Cache::keeping(64 << 10);
    let score = registered("gate-score", "https://x/gate-score", SCORE);
    let enrich = registered("gate-enrich", "https://x/gate-enrich", ENRICH);
    let images = cache
        .images_kept(&[score.clone(), enrich.clone()], &mut served, &mut keep)
        .unwrap();
    assert_eq!(
        (images[0].len(), images[1].len()),
        (SCORE.len() as u64, ENRICH.len() as u64)
    );
    assert!(!images[0].is_empty());
    // Whole pages of the cache and of its account.
    assert_eq!(cache.held(), 2 * 4096);
    assert_eq!(cache_account.usage().unwrap().used, 2 * 4096);
    // The second job is handed the same object, with no fetch and no new memory.
    let again = cache.image_kept(&score, &mut served, &mut keep).unwrap();
    assert!(Rc::ptr_eq(&again, &images[0]));
    assert_eq!(served.asked.len(), 2);
    assert_eq!(cache_account.usage().unwrap().used, 2 * 4096);
    // A job's handle reads the image and cannot write it.
    let granted = again.grant().unwrap();
    assert_eq!(read(&granted, SCORE.len()), SCORE);
    let (_, rights) = granted.info().unwrap();
    assert_eq!(rights, handover::GRANTED);
    assert!(!rights.contains(Rights::WRITE));
    // An image verified and then not kept (here, an account it may not charge) is refused as
    // unkept, and the cache keeps nothing of it.
    let readonly = moruna_svc::Account(
        cache_account
            .0
            .duplicate(Rights::READ | Rights::TRANSFER)
            .unwrap(),
    );
    let mut refuse = |bytes: Vec<u8>| MemoryImage::new(&bytes, &readonly.0).map(Rc::new);
    let mut roomy: Cache<Rc<MemoryImage>> = Cache::keeping(1 << 20);
    let refused = roomy
        .image_kept(&enrich, &mut served, &mut refuse)
        .unwrap_err();
    assert!(matches!(refused, Refused::Unkept { .. }), "{refused:?}");
    assert!(
        refused
            .to_string()
            .starts_with("the function `gate-enrich`'s image could not be kept: no memory object"),
        "{refused}"
    );
    assert!(roomy.is_empty());
    assert!(MemoryImage::new(b"", &cache_account.0).is_err());
}

#[test]
fn the_engine_is_given_each_image_under_its_slot_and_the_index_of_them() {
    let cache_account = account(1 << 20);
    let score = registered("gate-score", "https://x/gate-score", SCORE);
    let enrich = registered("gate-enrich", "https://x/gate-enrich", ENRICH);
    let images =
        [SCORE, ENRICH].map(|bytes| Rc::new(MemoryImage::new(bytes, &cache_account.0).unwrap()));
    let given = handover(&[score.clone(), enrich.clone()], &images).unwrap();
    let names: Vec<&str> = given.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["function.0", "function.1", "functions"]);
    for (_, handle) in &given {
        let (kind, rights) = handle.info().unwrap();
        assert_eq!(
            (kind, rights),
            (moruna_svc::ObjectType::Memory, handover::GRANTED)
        );
    }
    assert_eq!(read(&given[0].1, SCORE.len()), SCORE);
    assert_eq!(read(&given[1].1, ENRICH.len()), ENRICH);
    // The index the engine reads: its text, then a zero byte.
    let index = read(&given[2].1, 4096);
    let end = index.iter().position(|b| *b == 0).unwrap();
    let entries =
        node_protocol::functions::parse(core::str::from_utf8(&index[..end]).unwrap()).unwrap();
    assert_eq!(
        entries,
        [
            node_protocol::functions::Entry {
                name: "gate-score".into(),
                sha256: score.sha256,
                size: SCORE.len() as u64,
            },
            node_protocol::functions::Entry {
                name: "gate-enrich".into(),
                sha256: enrich.sha256,
                size: ENRICH.len() as u64,
            },
        ]
    );
    // A job that uses no function is given nothing; a mismatch or a name that is not a
    // registered function's is refused.
    assert!(handover(&[], &[]).unwrap().is_empty());
    assert!(
        handover(core::slice::from_ref(&score), &images)
            .unwrap_err()
            .contains("1 functions and 2")
    );
    let upper = registered("Gate", "https://x/gate", SCORE);
    assert!(
        handover(&[upper], &images[..1])
            .unwrap_err()
            .contains("index")
    );
    let many: Vec<Registered> = (0..=node_protocol::functions::MAX)
        .map(|n| registered(&format!("f{n}"), "https://x/f", SCORE))
        .collect();
    let copies: Vec<Rc<MemoryImage>> = many.iter().map(|_| images[0].clone()).collect();
    assert!(handover(&many, &copies).unwrap_err().contains("TooMany"));
}

#[test]
fn a_registered_sha256_is_64_lowercase_hex_digits() {
    let digest = hex(&sha256::digest(SCORE));
    assert!(Registered::new("a", "https://x/a", &digest).is_some());
    assert!(Registered::new("a", "https://x/a", &digest.to_uppercase()).is_none());
    assert!(Registered::new("a", "https://x/a", &digest[..62]).is_none());
    assert!(Registered::new("a", "https://x/a", &format!("{}zz", &digest[..62])).is_none());
}

#[test]
fn a_jobs_registrations_are_one_for_each_function_it_uses_in_order() {
    let digest = hex(&sha256::digest(SCORE));
    let entry = |name: &str, sha: &str| {
        (
            name.to_string(),
            format!("https://x/{name}"),
            sha.to_string(),
        )
    };
    let names = vec!["gate-score".to_string(), "gate-enrich".to_string()];
    let both = [entry("gate-score", &digest), entry("gate-enrich", &digest)];
    let taken = registrations(&names, &both).unwrap();
    assert_eq!(taken.len(), 2);
    assert_eq!(
        (taken[1].name.as_str(), taken[1].address.as_str()),
        ("gate-enrich", "https://x/gate-enrich")
    );
    assert!(registrations(&[], &[]).unwrap().is_empty());
    let fewer = registrations(&names, &both[..1]).unwrap_err();
    assert!(
        fewer.contains("uses 2 functions, and 1 registrations"),
        "{fewer}"
    );
    let swapped = registrations(&names, &[both[1].clone(), both[0].clone()]).unwrap_err();
    assert!(
        swapped.contains("`gate-score`, and the registration"),
        "{swapped}"
    );
    let bad = registrations(&names[..1], &[entry("gate-score", "XYZ")]).unwrap_err();
    assert!(bad.contains("no SHA-256"), "{bad}");
}
