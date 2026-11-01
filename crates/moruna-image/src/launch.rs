//! What an image's config and an entry's settings make of the image's process (SDD 19 e.1,
//! IM-I4; SDD 30 f.12): its command, its whole environment, its working directory and its
//! user. The sandbox on a Linux node and the engine on the Moruna kernel launch the same
//! process from the same image.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::source::Config;

/// The `PATH` a command is looked for in when the image's environment names none.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// An image's process, before it is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// The command: the entry's, or the image's entrypoint and cmd.
    pub command: Vec<String>,
    /// The image's environment with the entry's added, a later name replacing an earlier one;
    /// the process gets this and nothing of Moruna's.
    pub env: Vec<(String, String)>,
    /// The image's working directory, `/` when it names none.
    pub working_dir: String,
    /// The user and group it runs as: root, or the image's numeric uid and gid.
    pub uid: u32,
    /// See `uid`.
    pub gid: u32,
}

fn refuse(why: String) -> Error {
    Error::Format(why)
}

impl Launch {
    /// The process of image `name` with `config`, its command replaced by `command` and `env`
    /// added to its environment, as the entry's settings say.
    pub fn new(
        name: &str,
        config: &Config,
        command: Option<Vec<String>>,
        env: &BTreeMap<String, String>,
    ) -> Result<Launch> {
        let command = command.unwrap_or_else(|| config.command());
        if command.is_empty() {
            return Err(refuse(format!(
                "{name}: the image names no command (no Entrypoint and no Cmd) and the settings \
                 give none"
            )));
        }
        let (uid, gid) = identity(&config.user)?;
        let working_dir = if config.working_dir.is_empty() {
            "/".to_string()
        } else {
            config.working_dir.clone()
        };
        if !working_dir.starts_with('/') {
            return Err(refuse(format!(
                "{name}: the image's working directory `{working_dir}` is not absolute"
            )));
        }
        let launch = Launch {
            command,
            env: environment(&config.env, env)?,
            working_dir,
            uid,
            gid,
        };
        if launch
            .command
            .iter()
            .chain(launch.env.iter().flat_map(|(n, v)| [n, v]))
            .chain([&launch.working_dir])
            .any(|s| s.as_bytes().contains(&0))
        {
            return Err(refuse(format!(
                "{name}: the command, the environment or the working directory holds a NUL byte"
            )));
        }
        Ok(launch)
    }

    /// The `PATH` the command is looked for in.
    pub fn path(&self) -> &str {
        self.env
            .iter()
            .find(|(n, _)| n == "PATH")
            .map_or(DEFAULT_PATH, |(_, v)| v.as_str())
    }

    /// The paths the command is tried at, in order: itself when it names a directory, else
    /// one per `PATH` entry.
    pub fn candidates(&self) -> Vec<String> {
        let first = &self.command[0];
        if first.contains('/') {
            return vec![first.clone()];
        }
        self.path()
            .split(':')
            .map(|dir| {
                let dir = if dir.is_empty() { "." } else { dir };
                format!("{dir}/{first}")
            })
            .collect()
    }

    /// The environment as `NAME=value` entries.
    pub fn env_entries(&self) -> Vec<String> {
        self.env.iter().map(|(n, v)| format!("{n}={v}")).collect()
    }
}

/// The user an image runs as, from its config (SDD 19 IM-I4): root, or a numeric uid and gid.
fn identity(user: &str) -> Result<(u32, u32)> {
    let numeric = |s: &str| -> Result<u32> {
        s.parse::<u32>().map_err(|_| {
            refuse(format!(
                "the image's user `{user}` is not numeric: name its user by uid (and gid), for \
                 there is no passwd to look a name up in"
            ))
        })
    };
    match user {
        "" | "root" => Ok((0, 0)),
        _ => match user.split_once(':') {
            Some((u, g)) => Ok((numeric(u)?, numeric(g)?)),
            None => {
                let uid = numeric(user)?;
                Ok((uid, uid))
            }
        },
    }
}

/// The image's environment and the settings', each `NAME=value`, a later name replacing an
/// earlier one.
fn environment(
    image: &[String],
    settings: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, value: &str| match env.iter_mut().find(|(n, _)| n == name) {
        Some(entry) => entry.1 = value.to_string(),
        None => env.push((name.to_string(), value.to_string())),
    };
    for entry in image {
        let Some((name, value)) = entry.split_once('=') else {
            return Err(refuse(format!(
                "the image's Env entry `{entry}` is not NAME=value"
            )));
        };
        put(name, value);
    }
    for (name, value) in settings {
        put(name, value);
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(user: &str, dir: &str) -> Config {
        Config {
            entrypoint: vec![],
            cmd: vec!["python3".into(), "/kd2/transform.py".into()],
            env: vec!["PATH=/usr/local/bin:/usr/bin".into(), "LANG=C.UTF-8".into()],
            working_dir: dir.into(),
            user: user.into(),
        }
    }

    #[test]
    fn kd2_t13_the_config_makes_the_process_and_the_settings_amend_it() {
        let mut env = BTreeMap::new();
        env.insert("LANG".to_string(), "C".to_string());
        env.insert("HOME".to_string(), "/tmp".to_string());
        let l = Launch::new("img", &config("", ""), None, &env).unwrap();
        assert_eq!(l.command, ["python3", "/kd2/transform.py"]);
        assert_eq!(
            l.env_entries(),
            ["PATH=/usr/local/bin:/usr/bin", "LANG=C", "HOME=/tmp"]
        );
        assert_eq!((l.uid, l.gid, l.working_dir.as_str()), (0, 0, "/"));
        assert_eq!(
            l.candidates(),
            ["/usr/local/bin/python3", "/usr/bin/python3"]
        );
        let o = Launch::new(
            "img",
            &config("1000:100", "/work"),
            Some(vec!["/kc1/bin/tap".into()]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(o.candidates(), ["/kc1/bin/tap"]);
        assert_eq!((o.uid, o.gid, o.working_dir.as_str()), (1000, 100, "/work"));
    }

    #[test]
    fn kd2_t13_a_launch_that_cannot_be_is_refused() {
        let none = BTreeMap::new();
        let mut empty = config("", "");
        empty.cmd.clear();
        assert!(
            Launch::new("img", &empty, None, &none)
                .unwrap_err()
                .to_string()
                .contains("no command")
        );
        assert!(Launch::new("img", &config("nobody", ""), None, &none).is_err());
        assert!(Launch::new("img", &config("", "work"), None, &none).is_err());
        let mut bad = config("", "");
        bad.env.push("JUNK".into());
        assert!(Launch::new("img", &bad, None, &none).is_err());
    }

    /// IM-T27 (SDD 19 k), its launch's words: the user, the environment, the paths tried.
    #[test]
    fn im_t27_the_words_of_a_launch() {
        assert_eq!(identity("").unwrap(), (0, 0));
        assert_eq!(identity("root").unwrap(), (0, 0));
        assert_eq!(identity("1000").unwrap(), (1000, 1000));
        assert_eq!(identity("1000:50").unwrap(), (1000, 50));
        let e = identity("app").unwrap_err().to_string();
        assert!(e.contains("user `app` is not numeric"), "{e}");
        assert!(identity("1000:staff").is_err());

        let image = vec![
            "PATH=/bin".to_string(),
            "A=1".to_string(),
            "B=x=y".to_string(),
        ];
        let mut settings = BTreeMap::new();
        settings.insert("A".to_string(), "2".to_string());
        settings.insert("C".to_string(), "3".to_string());
        assert_eq!(
            environment(&image, &settings).unwrap(),
            [("PATH", "/bin"), ("A", "2"), ("B", "x=y"), ("C", "3")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
        );
        let e = environment(&["NOEQUALS".to_string()], &settings)
            .unwrap_err()
            .to_string();
        assert!(e.contains("`NOEQUALS` is not NAME=value"), "{e}");

        let paths = |c: &str, p: &str| -> Vec<String> {
            let config = Config {
                cmd: vec![c.to_string()],
                env: vec![format!("PATH={p}")],
                ..Config::default()
            };
            Launch::new("img", &config, None, &BTreeMap::new())
                .unwrap()
                .candidates()
        };
        assert_eq!(paths("/bin/x", "/a"), ["/bin/x"]);
        assert_eq!(paths("./x", "/a"), ["./x"]);
        assert_eq!(paths("x", "/a::/b"), ["/a/x", "./x", "/b/x"]);
        let nul = Config {
            cmd: vec!["a\0b".to_string()],
            ..Config::default()
        };
        assert!(Launch::new("img", &nul, None, &BTreeMap::new()).is_err());
    }
}
