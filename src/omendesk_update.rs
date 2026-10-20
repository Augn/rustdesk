use crate::{
    common::{display_version_from_release_id, SOFTWARE_UPDATE_URL, SOFTWARE_UPDATE_VERSION},
    hbbs_http::{create_http_client_async, get_url_for_tls},
};
use hbb_common::{
    anyhow::anyhow,
    config::Config,
    get_version_number,
    tls::{get_cached_tls_type, upsert_tls_cache, TlsType},
    ResultType,
};
use serde_json::Value;
#[cfg(feature = "flutter")]
use std::collections::HashMap;

const SOFTWARE_UPDATE_RELEASE_API: &str =
    "https://api.github.com/repos/Augn/rustdesk/releases/latest";

pub(super) async fn check_software_update() -> ResultType<()> {
    *SOFTWARE_UPDATE_URL.lock().unwrap() = String::new();
    *SOFTWARE_UPDATE_VERSION.lock().unwrap() = String::new();
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(SOFTWARE_UPDATE_RELEASE_API, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url);
    let is_tls_not_cached = tls_type.is_none();
    let tls_type = tls_type.unwrap_or(TlsType::Rustls);
    let client = create_http_client_async(tls_type, false);
    let send_request = |client: &reqwest::Client| {
        client
            .get(SOFTWARE_UPDATE_RELEASE_API)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "omendesk-updater")
    };
    let response = match send_request(&client).send().await {
        Ok(resp) => {
            upsert_tls_cache(tls_url, tls_type, false);
            resp
        }
        Err(err) => {
            if is_tls_not_cached && err.is_request() {
                let tls_type = TlsType::NativeTls;
                let client = create_http_client_async(tls_type, false);
                let resp = send_request(&client).send().await?;
                upsert_tls_cache(tls_url, tls_type, false);
                resp
            } else {
                return Err(err.into());
            }
        }
    };
    let release: Value = response.error_for_status()?.json().await?;
    if let Some((version, release_url)) = software_update_release(&release, crate::VERSION)? {
        *SOFTWARE_UPDATE_VERSION.lock().unwrap() = version;
        *SOFTWARE_UPDATE_URL.lock().unwrap() = release_url.clone();

        #[cfg(feature = "flutter")]
        {
            let release_notes = release
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let event = HashMap::from([
                ("name", "check_software_update_finish"),
                ("url", release_url.as_str()),
                ("content", release_notes),
            ]);
            if let Ok(data) = serde_json::to_string(&event) {
                let _ = crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, data);
            }
        }
    }
    Ok(())
}

fn software_update_release(
    release: &Value,
    current_version: &str,
) -> ResultType<Option<(String, String)>> {
    if release.get("draft").and_then(Value::as_bool) != Some(false)
        || release.get("prerelease").and_then(Value::as_bool) != Some(false)
    {
        return Ok(None);
    }
    let release_id = release
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Update release has no tag name"))?;
    let version = display_version_from_release_id(release_id)?;
    if get_version_number(&version) <= get_version_number(current_version) {
        return Ok(None);
    }
    let release_url = format!("https://github.com/Augn/rustdesk/releases/tag/{release_id}");
    Ok(Some((version, release_url)))
}

#[cfg(test)]
mod software_update_tests {
    use super::software_update_release;
    use serde_json::json;

    #[test]
    fn discovers_stable_omendesk_release_at_immutable_tag() {
        let release = json!({
            "tag_name": "v1.5.2", "draft": false, "prerelease": false
        });
        assert_eq!(
            software_update_release(&release, "1.5.1").unwrap(),
            Some((
                "1.5.2".to_owned(),
                "https://github.com/Augn/rustdesk/releases/tag/v1.5.2".to_owned()
            ))
        );
        assert!(software_update_release(&release, "1.5.2")
            .unwrap()
            .is_none());
        assert!(software_update_release(&release, "1.5.3")
            .unwrap()
            .is_none());
    }

    #[test]
    fn excludes_unpublished_prerelease_and_mutable_channels() {
        for (draft, prerelease) in [(true, false), (false, true)] {
            assert!(software_update_release(
                &json!({
                    "tag_name": "1.5.2", "draft": draft, "prerelease": prerelease
                }),
                "1.5.1"
            )
            .unwrap()
            .is_none());
        }
        for tag in ["master", "nightly", "1.5.2-rc1"] {
            assert!(software_update_release(
                &json!({
                    "tag_name": tag, "draft": false, "prerelease": false
                }),
                "1.5.1"
            )
            .is_err());
        }
    }
}
