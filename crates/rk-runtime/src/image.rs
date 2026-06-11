//! OCI image reference parsing with Docker-style defaulting: bare names get
//! the `docker.io` registry, the `library/` repository prefix and the
//! `latest` tag, so `nginx` means `docker.io/library/nginx:latest`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid image reference {input:?}: {reason}")]
pub struct ImageRefError {
    pub input: String,
    pub reason: &'static str,
}

impl ImageRefError {
    fn new(input: &str, reason: &'static str) -> Self {
        ImageRefError {
            input: input.to_owned(),
            reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reference {
    Tag(String),
    /// `sha256:<hex>` — pins the exact manifest.
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef {
    /// Logical registry name, e.g. `docker.io`, `ghcr.io`, `localhost:5000`.
    pub registry: String,
    /// Repository path, e.g. `library/nginx`.
    pub repository: String,
    pub reference: Reference,
}

impl ImageRef {
    /// Host to contact for the registry API. Docker Hub's logical name and
    /// its API endpoint differ; everything else is contacted as named.
    pub fn registry_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }
}

impl FromStr for ImageRef {
    type Err = ImageRefError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ImageRefError::new(input, "empty reference"));
        }

        // 1. Split off a digest. A digest pins the manifest, so any tag left
        //    in the name part is informational and discarded.
        let (name_part, digest) = match trimmed.split_once('@') {
            Some((name, digest)) => {
                if !digest.starts_with("sha256:") || digest.len() <= "sha256:".len() {
                    return Err(ImageRefError::new(
                        input,
                        "only sha256: digests are supported",
                    ));
                }
                (name, Some(digest.to_owned()))
            }
            None => (trimmed, None),
        };

        // 2. The first path segment is a registry only if it looks like a
        //    host (contains a dot or port, or is `localhost`); otherwise the
        //    whole name is a Docker Hub repository.
        let (registry, remainder) = match name_part.split_once('/') {
            Some((first, tail))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_owned(), tail)
            }
            _ => ("docker.io".to_owned(), name_part),
        };

        // 3. With the registry (and its possible port colon) gone, a
        //    remaining colon separates the tag.
        let (repository, tag) = match remainder.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => {
                if tag.is_empty() {
                    return Err(ImageRefError::new(input, "empty tag"));
                }
                (repo.to_owned(), Some(tag.to_owned()))
            }
            _ => (remainder.to_owned(), None),
        };
        if repository.is_empty() {
            return Err(ImageRefError::new(input, "empty repository"));
        }

        // 4. Docker Hub official images live under `library/`.
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };

        let reference = match digest {
            Some(digest) => Reference::Digest(digest),
            None => Reference::Tag(tag.unwrap_or_else(|| "latest".to_owned())),
        };

        Ok(ImageRef {
            registry,
            repository,
            reference,
        })
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        match &self.reference {
            Reference::Tag(tag) => write!(f, ":{tag}"),
            Reference::Digest(digest) => write!(f, "@{digest}"),
        }
    }
}

impl Serialize for ImageRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ImageRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ImageRef {
        s.parse().unwrap()
    }

    #[test]
    fn docker_hub_shorthand_expands() {
        let cases = [
            ("nginx", "docker.io", "library/nginx", "latest"),
            ("nginx:1.27", "docker.io", "library/nginx", "1.27"),
            ("library/nginx", "docker.io", "library/nginx", "latest"),
            ("grafana/loki:2.9", "docker.io", "grafana/loki", "2.9"),
        ];
        for (input, registry, repository, tag) in cases {
            let image = parse(input);
            assert_eq!(image.registry, registry, "{input}");
            assert_eq!(image.repository, repository, "{input}");
            assert_eq!(image.reference, Reference::Tag(tag.to_owned()), "{input}");
        }
    }

    #[test]
    fn explicit_registries_are_respected() {
        let image = parse("ghcr.io/owner/app:v1");
        assert_eq!(image.registry, "ghcr.io");
        assert_eq!(image.repository, "owner/app");
        assert_eq!(image.reference, Reference::Tag("v1".to_owned()));

        let local = parse("localhost:5000/myimg");
        assert_eq!(local.registry, "localhost:5000");
        assert_eq!(local.repository, "myimg");
        assert_eq!(local.reference, Reference::Tag("latest".to_owned()));

        let nested = parse("registry.k8s.io/coredns/coredns:v1.11.1");
        assert_eq!(nested.registry, "registry.k8s.io");
        assert_eq!(nested.repository, "coredns/coredns");
    }

    #[test]
    fn digests_pin_the_reference() {
        let image = parse("nginx@sha256:abcdef0123456789");
        assert_eq!(image.repository, "library/nginx");
        assert_eq!(
            image.reference,
            Reference::Digest("sha256:abcdef0123456789".to_owned())
        );

        // A tag alongside a digest is discarded — the digest pins it.
        let pinned = parse("ghcr.io/owner/app:v1@sha256:abcdef0123456789");
        assert_eq!(pinned.repository, "owner/app");
        assert!(matches!(pinned.reference, Reference::Digest(_)));
    }

    #[test]
    fn docker_hub_uses_its_api_host() {
        assert_eq!(parse("nginx").registry_host(), "registry-1.docker.io");
        assert_eq!(parse("ghcr.io/o/a").registry_host(), "ghcr.io");
    }

    #[test]
    fn display_round_trips() {
        for input in [
            "nginx",
            "nginx:1.27",
            "ghcr.io/owner/app:v1",
            "localhost:5000/myimg:dev",
            "nginx@sha256:abcdef0123456789",
        ] {
            let image = parse(input);
            let again: ImageRef = image.to_string().parse().unwrap();
            assert_eq!(image, again, "{input}");
        }
    }

    #[test]
    fn invalid_references_are_rejected() {
        for bad in [
            "",
            "  ",
            "nginx:",
            "docker.io/",
            "nginx@md5:abc",
            "nginx@sha256:",
        ] {
            assert!(bad.parse::<ImageRef>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn image_ref_serializes_as_a_string() {
        let image = parse("nginx:1.27");
        let json = serde_json::to_string(&image).unwrap();
        assert_eq!(json, r#""docker.io/library/nginx:1.27""#);
        let back: ImageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(image, back);
    }
}
