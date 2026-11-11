//! SAML 2.0 single sign-on (service provider side): SP-initiated, HTTP-Redirect to the identity
//! provider and HTTP-POST back to the assertion consumer service.
//!
//! Signature and assertion validation are done by `samael` on top of the xmlsec1 C library, which
//! is why SAML is a cargo feature (`saml`, on by default): building it needs libxml2 and xmlsec1.
//! Responses must be signed by a certificate from the pasted IdP metadata, answer a request this
//! server issued (IdP-initiated sign-in is not accepted), and name this SP as their audience.
use crate::db::{get_setting, Db};
use crate::AppState;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SamlConfig {
    pub enabled: bool,
    /// The identity provider's metadata XML (pasted, or fetched from `idp_metadata_url` on save).
    pub idp_metadata: String,
    pub idp_metadata_url: String,
    /// PEM bundle trusted when fetching the metadata URL.
    pub ca_cert: String,
    /// This service provider's entity ID. Blank = the metadata URL.
    pub entity_id: String,
    pub attr_username: String,
    pub attr_display: String,
    pub attr_email: String,
    pub attr_groups: String,
    pub button_label: String,
}

impl SamlConfig {
    pub fn usable(&self) -> bool {
        AVAILABLE && self.enabled && !self.idp_metadata.trim().is_empty()
    }
    pub fn groups_attribute(&self) -> &str {
        if self.attr_groups.trim().is_empty() { "groups" } else { self.attr_groups.trim() }
    }
}

pub async fn load_config(db: &Db) -> SamlConfig {
    get_setting(db, "saml").await
}

pub fn acs_url(state: &AppState) -> String {
    format!("{}/auth/saml/acs", state.cfg.public_url)
}

pub fn metadata_url(state: &AppState) -> String {
    format!("{}/auth/saml/metadata", state.cfg.public_url)
}

pub fn entity_id(state: &AppState, cfg: &SamlConfig) -> String {
    if cfg.entity_id.trim().is_empty() { metadata_url(state) } else { cfg.entity_id.trim().to_string() }
}

/// This service provider's metadata document, for registering Autokan with the identity provider.
/// It advertises exactly what is implemented: one HTTP-POST assertion consumer service, unsigned
/// requests, signed assertions required, and no single logout.
pub fn sp_metadata(state: &AppState, cfg: &SamlConfig) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <md:EntityDescriptor xmlns:md=\"urn:oasis:names:tc:SAML:2.0:metadata\" entityID=\"{}\">\n  \
         <md:SPSSODescriptor AuthnRequestsSigned=\"false\" WantAssertionsSigned=\"true\" protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\">\n    \
         <md:AssertionConsumerService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" Location=\"{}\" index=\"0\" isDefault=\"true\"/>\n  \
         </md:SPSSODescriptor>\n\
         </md:EntityDescriptor>\n",
        esc(&entity_id(state, cfg)),
        esc(&acs_url(state))
    )
}

/// Who a validated assertion says signed in.
pub struct SamlUser {
    pub idp: String,
    pub name_id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

#[cfg(feature = "saml")]
mod provider {
    use super::*;
    use samael::crypto::AllowedSignatureAlgorithm as Alg;
    use samael::metadata::EntityDescriptor;
    use samael::service_provider::{ServiceProvider, ServiceProviderBuilder};

    pub const AVAILABLE: bool = true;
    const REDIRECT_BINDING: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect";

    fn service_provider(state: &AppState, cfg: &SamlConfig) -> Result<ServiceProvider, String> {
        let idp: EntityDescriptor = cfg
            .idp_metadata
            .trim()
            .parse()
            .map_err(|e| format!("The IdP metadata could not be read: {e}"))?;
        if idp.entity_id.as_deref().unwrap_or("").is_empty() {
            return Err("The IdP metadata has no entityID".into());
        }
        let sp = ServiceProviderBuilder::default()
            .entity_id(entity_id(state, cfg))
            .idp_metadata(idp)
            .acs_url(acs_url(state))
            .metadata_url(metadata_url(state))
            .slo_url(None::<String>)
            .allow_idp_initiated(false)
            // SHA-1 signatures are refused.
            .allowed_signature_algorithms(Some(vec![
                Alg::RsaSha256,
                Alg::RsaSha384,
                Alg::RsaSha512,
                Alg::EcdsaSha256,
                Alg::EcdsaSha384,
                Alg::EcdsaSha512,
            ]))
            .build()
            .map_err(|e| format!("Invalid SAML settings: {e}"))?;
        // Without a signing certificate samael would accept unsigned responses. Never allow that.
        match sp.idp_signing_certs() {
            Ok(Some(certs)) if !certs.is_empty() => Ok(sp),
            Ok(_) => Err("The IdP metadata contains no signing certificate, so responses could not be verified".into()),
            Err(e) => Err(format!("The signing certificate in the IdP metadata could not be read: {e}")),
        }
    }

    /// Check that the settings are complete enough to sign people in.
    pub fn validate(state: &AppState, cfg: &SamlConfig) -> Result<(), String> {
        let sp = service_provider(state, cfg)?;
        sp.sso_binding_location(REDIRECT_BINDING)
            .map(|_| ())
            .ok_or_else(|| "The IdP metadata has no HTTP-Redirect SingleSignOnService endpoint".to_string())
    }

    /// Build an AuthnRequest. Returns (request id, URL to send the browser to).
    pub fn authn_request(state: &AppState, cfg: &SamlConfig, relay_state: &str) -> Result<(String, String), String> {
        let sp = service_provider(state, cfg)?;
        let sso = sp
            .sso_binding_location(REDIRECT_BINDING)
            .ok_or("The IdP metadata has no HTTP-Redirect SingleSignOnService endpoint")?;
        let mut request = sp.make_authentication_request(&sso).map_err(|e| format!("Could not build the SAML request: {e}"))?;
        // The response is matched to this id, so it must be unguessable.
        request.id = format!("id-{}", crate::auth::random_token(20));
        let url = request
            .redirect(relay_state)
            .map_err(|e| format!("Could not encode the SAML request: {e}"))?
            .ok_or("The SAML request has no destination")?;
        Ok((request.id, url.to_string()))
    }

    /// Validate a base64 SAMLResponse that must answer `request_id`.
    pub fn consume(state: &AppState, cfg: &SamlConfig, response: &str, request_id: &str) -> Result<SamlUser, String> {
        let sp = service_provider(state, cfg)?;
        let assertion = sp
            .parse_base64_response(response.trim(), Some(&[request_id]))
            .map_err(|e| format!("The SAML response was rejected: {e}"))?;
        let name_id = assertion
            .subject
            .as_ref()
            .and_then(|s| s.name_id.as_ref())
            .map(|n| n.value.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or("The SAML assertion has no NameID")?;
        let values = |wanted: &str| -> Vec<String> {
            let wanted = wanted.trim();
            if wanted.is_empty() {
                return Vec::new();
            }
            assertion
                .attribute_statements
                .iter()
                .flatten()
                .flat_map(|s| s.attributes.iter())
                .filter(|a| a.name.as_deref() == Some(wanted) || a.friendly_name.as_deref() == Some(wanted))
                .flat_map(|a| a.values.iter().filter_map(|v| v.value.clone()))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect()
        };
        let username = values(&cfg.attr_username).into_iter().next().unwrap_or_else(|| name_id.clone());
        Ok(SamlUser {
            idp: sp.idp_metadata.entity_id.clone().unwrap_or_default(),
            display_name: values(&cfg.attr_display).into_iter().next().unwrap_or_else(|| username.clone()),
            email: values(&cfg.attr_email).into_iter().next(),
            groups: values(cfg.groups_attribute()),
            username,
            name_id,
        })
    }

}

#[cfg(not(feature = "saml"))]
mod provider {
    use super::*;
    pub const AVAILABLE: bool = false;
    const MISSING: &str = "This build of Autokan does not include SAML support (rebuild with the `saml` feature)";

    pub fn validate(_: &AppState, _: &SamlConfig) -> Result<(), String> {
        Err(MISSING.into())
    }
    pub fn authn_request(_: &AppState, _: &SamlConfig, _: &str) -> Result<(String, String), String> {
        Err(MISSING.into())
    }
    pub fn consume(_: &AppState, _: &SamlConfig, _: &str, _: &str) -> Result<SamlUser, String> {
        Err(MISSING.into())
    }
}

pub use provider::{authn_request, consume, validate, AVAILABLE};
