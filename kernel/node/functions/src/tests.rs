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

#[test]
fn a_registered_sha256_is_64_lowercase_hex_digits() {
    let digest = hex(&sha256::digest(SCORE));
    assert!(Registered::new("a", "https://x/a", &digest).is_some());
    assert!(Registered::new("a", "https://x/a", &digest.to_uppercase()).is_none());
    assert!(Registered::new("a", "https://x/a", &digest[..62]).is_none());
    assert!(Registered::new("a", "https://x/a", &format!("{}zz", &digest[..62])).is_none());
}
