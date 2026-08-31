//! The one part of this sensor that leaves the host (#819).
//!
//! It answers two questions a local socket cannot: **is the pinned digest
//! still what this tag resolves to upstream?** — the live gauge that replaces
//! `image-update-report.sh` and its monthly mail — and **does a signature
//! exist for the digest we are running?**, which is what cosign silently
//! signing nothing for eight days looked like from outside.
//!
//! It is off by default and, when on, restricted to a **named allowlist of
//! registries**: an allowlist that defaults to "everything" is not one, and a
//! monitoring agent should not decide on its own which third-party hosts to
//! contact. `validate()` refuses `enabled` without `registries`.
//!
//! Only anonymous, read-only registry-v2 requests are made. No credentials are
//! read, sent, or stored — a private registry simply answers 401 and the
//! result is `None`, which is honest.

use std::time::Duration;

use crate::config::UpstreamConfig;

/// A parsed image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl ImageRef {
    /// The registry-v2 base for this image.
    ///
    /// Docker Hub is the one registry whose *name* and whose *API host* differ
    /// (`docker.io` → `registry-1.docker.io`), and whose short references
    /// imply the `library/` namespace. Everything else is literal.
    pub fn api_host(&self) -> &str {
        match self.registry.as_str() {
            "docker.io" | "index.docker.io" => "registry-1.docker.io",
            other => other,
        }
    }
}

/// Parse `[registry/]repository[:tag][@digest]`.
///
/// A reference pinned by digest returns `None`: there is no tag to resolve, so
/// "is it behind?" is not a question with an answer — and a pinned digest is
/// *supposed* to stay put.
pub fn parse_reference(reference: &str) -> Option<ImageRef> {
    // Strip any digest suffix; what is left is the tag-addressed part.
    let (head, had_digest) = match reference.split_once('@') {
        Some((h, _)) => (h, true),
        None => (reference, false),
    };
    if had_digest && !head.contains(':') {
        return None;
    }
    let (path, tag) = match head.rsplit_once(':') {
        // A colon in the first segment is a port, not a tag
        // (`localhost:5000/img`).
        Some((p, t)) if !t.contains('/') => (p, t.to_string()),
        _ => (head, "latest".to_string()),
    };
    let (registry, repository) = match path.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first.to_string(), rest.to_string())
        }
        // No registry component: Docker Hub, and a bare name is in `library/`.
        _ => (
            "docker.io".to_string(),
            if path.contains('/') {
                path.to_string()
            } else {
                format!("library/{path}")
            },
        ),
    };
    Some(ImageRef {
        registry,
        repository,
        tag,
    })
}

pub struct UpstreamChecker {
    http: reqwest::Client,
    allow: Vec<String>,
    signatures: bool,
}

impl UpstreamChecker {
    pub fn new(cfg: &UpstreamConfig, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(concat!(
                    "zensight-sensor-container/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()?,
            allow: cfg.registries.clone(),
            signatures: cfg.signatures,
        })
    }

    fn allowed(&self, r: &ImageRef) -> bool {
        self.allow.iter().any(|a| a == &r.registry)
    }

    /// The digest this tag resolves to now, or `None` for anything the check
    /// cannot answer — not on the allowlist, needs auth, does not exist.
    /// `None` must never be read as "up to date".
    pub async fn digest_for(&self, reference: &str) -> Option<String> {
        let r = parse_reference(reference)?;
        if !self.allowed(&r) {
            return None;
        }
        let url = format!(
            "https://{}/v2/{}/manifests/{}",
            r.api_host(),
            r.repository,
            r.tag
        );
        let resp = self
            .http
            .head(&url)
            .header(
                "Accept",
                "application/vnd.oci.image.index.v1+json, \
                 application/vnd.oci.image.manifest.v1+json, \
                 application/vnd.docker.distribution.manifest.list.v2+json, \
                 application/vnd.docker.distribution.manifest.v2+json",
            )
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Whether a cosign signature object exists for `digest`.
    ///
    /// cosign stores it at the tag `sha256-<hex>.sig` in the same repository,
    /// so presence is a HEAD away. This proves a signature **exists**, not
    /// that it verifies — verification needs the public key and belongs in a
    /// tool that has one. Saying "present" is therefore the strongest honest
    /// claim, and it is exactly the claim that would have caught cosign
    /// signing nothing for eight days.
    pub async fn signature_present(&self, reference: &str, digest: &str) -> Option<bool> {
        if !self.signatures {
            return None;
        }
        let r = parse_reference(reference)?;
        if !self.allowed(&r) {
            return None;
        }
        let tag = format!("{}.sig", digest.replace(':', "-"));
        let url = format!(
            "https://{}/v2/{}/manifests/{tag}",
            r.api_host(),
            r.repository
        );
        let resp = self.http.head(&url).send().await.ok()?;
        match resp.status().as_u16() {
            200 => Some(true),
            404 => Some(false),
            // 401/403/5xx: the registry did not answer the question. Silence,
            // not "unsigned".
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_is_docker_hubs_library_namespace() {
        let r = parse_reference("caddy:2.11.4").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/caddy");
        assert_eq!(r.tag, "2.11.4");
        assert_eq!(r.api_host(), "registry-1.docker.io");
    }

    #[test]
    fn a_fully_qualified_reference_is_taken_literally() {
        let r = parse_reference("git.marcpardo.eu/marcpardo/zensight-sensor-pve:0.12.0").unwrap();
        assert_eq!(r.registry, "git.marcpardo.eu");
        assert_eq!(r.repository, "marcpardo/zensight-sensor-pve");
        assert_eq!(r.tag, "0.12.0");
        assert_eq!(r.api_host(), "git.marcpardo.eu");
    }

    #[test]
    fn a_missing_tag_is_latest() {
        assert_eq!(
            parse_reference("docker.io/library/nginx").unwrap().tag,
            "latest"
        );
    }

    /// A colon in the first segment is a PORT. Reading it as a tag turns
    /// `localhost:5000/img` into the image `localhost` at tag `5000/img`.
    #[test]
    fn a_registry_port_is_not_a_tag() {
        let r = parse_reference("localhost:5000/img:v1").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "img");
        assert_eq!(r.tag, "v1");
    }

    /// A digest-pinned reference has no tag to resolve, and is *supposed* to
    /// stay put. Asking "is it behind?" of it is a category error.
    #[test]
    fn a_digest_pinned_reference_has_no_question_to_ask() {
        assert_eq!(parse_reference("caddy@sha256:aaaa"), None);
        // …unless a tag is also present, in which case the tag is the question.
        assert_eq!(
            parse_reference("caddy:2.11.4@sha256:aaaa").unwrap().tag,
            "2.11.4"
        );
    }

    /// Nothing off the allowlist is ever contacted — the check returns `None`
    /// before any request is built.
    #[tokio::test]
    async fn a_registry_off_the_allowlist_is_never_contacted() {
        let c = UpstreamChecker::new(
            &UpstreamConfig {
                enabled: true,
                registries: vec!["git.marcpardo.eu".into()],
                ..Default::default()
            },
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(c.digest_for("docker.io/library/nginx:latest").await, None);
    }

    #[tokio::test]
    async fn signature_checking_answers_nothing_while_switched_off() {
        let c = UpstreamChecker::new(
            &UpstreamConfig {
                enabled: true,
                signatures: false,
                registries: vec!["docker.io".into()],
                ..Default::default()
            },
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(
            c.signature_present("caddy:2.11.4", "sha256:aaa").await,
            None
        );
    }
}
