use crate::error::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::convert::{TryFrom, TryInto};
use uriparse::{URI, Query, Fragment, Authority};

pub const SEPARATOR: char = '/';
pub const SEPARATOR_CHAR: char = '/';
pub const CUR_DIR: &'static str = ".";
pub static WINDOWS: Lazy<bool> = Lazy::new(|| if cfg!(windows) { true } else { false });

static HAS_URI_SCHEME: Lazy<Regex> = Lazy::new(|| Regex::new("[a-zA-Z][a-zA-Z0-9+-.]+:").unwrap());
static HAS_DRIVE_LETTER_SPECIFIER: Lazy<Regex> = Lazy::new(|| Regex::new("^/?[a-zA-Z]:").unwrap());

#[derive(Eq, PartialEq,  Debug)]
pub struct Path {
    url: URI<'static>,
}

impl Path {
    pub fn from_url(url: URI<'static>) -> Self {
        Path { url }
    }

    pub fn from_path_string(path_string: &str) -> Self {
        // TODO can't directly parse as string might not be escaped
        let mut path_string: String = path_string.to_string();
        if path_string.len() == 0 {
            panic!("can not create a Path from an empty string");
        }
        if Self::has_windows_drive(&path_string) && !path_string.starts_with('/') {
            path_string = format!("/{:?}", path_string);
        }
        let mut scheme= None;
        let mut authority = None;
        let mut index = 0;

        // parse uri scheme if any present
        let colon = path_string.find(':');
        let slash = path_string.find('/');
        if colon.is_some() && (slash.is_none() || colon.unwrap() < slash.unwrap()) {
            scheme = Some(path_string.get(0..colon.unwrap()).unwrap());
            index = colon.unwrap() + 1;
        }

        // parse uri authority if any
        println!("index after scheme {}", index);
        if path_string.get(index..).unwrap().starts_with("//") && path_string.len() - index > 2 {
            println!("{:?}", path_string.get(index + 2..));
            let next_slash = path_string[index+2..].find('/').map(|fi| index +2 +fi).unwrap();
            println!("next slash {}", next_slash);
            let auth_end = if next_slash > 0 {
                next_slash
            } else {
                path_string.len()
            };
            println!("auth end {}", auth_end);
            authority = Some(path_string.get(index +2..auth_end).unwrap());
            index = auth_end;
        }
        println!("index after authority {}", index);

        let path = path_string.get(index .. path_string.len());

        println!("scheme {:?}", scheme);
        println!("path {:?}", path);
        println!("authority {:?}", authority);
        let mut url = URI::from_parts(scheme.unwrap(),None::<Authority>,  path.unwrap(), None::<Query>, None::<Fragment>).unwrap();
        println!("url {:?}", url.to_string());
        url.normalize();
        println!("normalized url {:?}", url.to_string());

        Path { url: url.into_owned() }
    }

    pub fn to_url(&self) -> &URI {
        &self.url
    }

    pub fn is_url_path_absolute(&self) -> bool {
        let start = self.start_position_without_windows_drive(&self.url.path().to_string());
        self.url.path().to_string().get(start..).unwrap().starts_with(SEPARATOR)
    }

    pub fn is_absolute(&self) -> bool {
        self.is_url_path_absolute()
    }

    fn start_position_without_windows_drive(&self, path: &str) -> usize {
        if Self::has_windows_drive(path) {
            if path.chars().next().unwrap() == SEPARATOR {
                3
            } else {
                2
            }
        } else {
            0
        }
    }

    fn has_windows_drive(path: &str) -> bool {
        *WINDOWS && HAS_DRIVE_LETTER_SPECIFIER.find(path).is_some()
    }

    pub fn get_name(&self) -> Option<String> {
        let path = self.url.path();
        let slash = path.to_string().rfind(SEPARATOR)?;
        path.to_string().get(slash + 1..).map(|x| x.to_owned())
    }

    pub fn get_parent(&self) -> Option<Path> {
        let path = self.url.path();
        let last_slash = path.to_string().rfind(SEPARATOR);
        let start = self.start_position_without_windows_drive(&path.to_string());
        if (path.to_string().len() == start) || (last_slash? == start && path.to_string().len() == start + 1) {
            return None;
        }
        let parent_path = if last_slash.is_none() {
            CUR_DIR.to_string()
        } else {
            path.to_string().get(
                0..if last_slash? == start {
                    start + 1
                } else {
                    last_slash?
                },
            ).map(|x| x.to_string())?
        };
        let mut parent = self.url.clone();
        parent.set_path(parent_path.as_str());
        println!("parent {}", parent);
        Some(Path::from_url(parent.into_owned()))
    }

    pub fn is_root(&self) -> bool {
        self.get_parent().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_name() {
        assert_eq!("", Path::from_path_string("/").get_name().unwrap());
        assert_eq!("foo", Path::from_path_string("foo").get_name().unwrap());
        assert_eq!("foo", Path::from_path_string("/foo").get_name().unwrap());
        assert_eq!("foo", Path::from_path_string("/foo/").get_name().unwrap());
        assert_eq!(
            "bar",
            Path::from_path_string("/foo/bar").get_name().unwrap()
        );
        assert_eq!(
            "bar",
            Path::from_path_string("hdfs://host/foo/bar")
                .get_name()
                .unwrap()
        );
    }

    #[test]
    fn is_absolute() {
        assert!(Path::from_path_string("/").is_absolute());
        assert!(Path::from_path_string("/foo").is_absolute());
        assert!(!Path::from_path_string("foo").is_absolute());
        assert!(!Path::from_path_string("foo/bar").is_absolute());
        assert!(!Path::from_path_string(".").is_absolute());
        assert!(Path::from_path_string("scheme:///foo/bar").is_absolute());

        if *WINDOWS {
            assert!(Path::from_path_string("C:/a/b").is_absolute());
            assert!(Path::from_path_string("C:a/b").is_absolute());
        }
    }

    #[test]
    fn parent() {
        assert_eq!(
            Path::from_path_string("file:///foo"),
            Path::from_path_string("file:///foo/bar").get_parent().unwrap()
        );
        println!("-----");
        assert_eq!(
            Path::from_path_string("foo"),
            Path::from_path_string("foo/bar").get_parent().unwrap()
        );
        assert_eq!(
            Path::from_path_string("/"),
            Path::from_path_string("/foo").get_parent().unwrap()
        );
        assert!(Path::from_path_string("/").get_parent().is_none());

        if *WINDOWS {
            assert_eq!(
                Path::from_path_string("c:/"),
                Path::from_path_string("c:/foo").get_parent().unwrap()
            );
        }
    }
}
