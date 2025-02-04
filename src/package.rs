use crate::guac::Guac;
use crate::sbom::SbomRegistry;
use crate::Snyk;
use actix_web::http::header::{DispositionParam, DispositionType};
use actix_web::{
    error, get,
    http::{header::ContentDisposition, StatusCode},
    post, web,
    web::Json,
    web::ServiceConfig,
    HttpResponse,
};
use core::str::FromStr;
use packageurl::PackageUrl;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

pub use trust_api_model::pkg::*;

pub(crate) fn configure() -> impl FnOnce(&mut ServiceConfig) {
    |config: &mut ServiceConfig| {
        config.service(get_package);
        config.service(query_package);
        config.service(query_package_dependencies);
        config.service(query_package_dependents);
        config.service(get_trusted);
        config.service(query_package_versions);
        config.service(query_sbom);
    }
}

#[derive(serde::Deserialize)]
pub struct PackageQuery {
    purl: Option<String>,
}

pub struct TrustedContent {
    sbom: Arc<SbomRegistry>,
    client: Arc<Guac>,
    snyk: Snyk,
}

impl TrustedContent {
    pub fn new(client: Arc<Guac>, sbom: Arc<SbomRegistry>, snyk: Snyk) -> Self {
        Self { client, snyk, sbom }
    }

    pub async fn get_versions(&self, purl_str: &str) -> Result<Vec<PackageRef>, ApiError> {
        if let Ok(purl) = PackageUrl::from_str(purl_str) {
            let trusted_versions: Vec<PackageRef> = self
                .client
                .get_packages(purl.clone())
                .await
                .map_err(|_| ApiError::InternalError)?;

            Ok(trusted_versions)
        } else {
            Err(ApiError::InvalidPackageUrl {
                purl: purl_str.to_string(),
            })
        }
    }

    async fn get_trusted(&self, purl_str: &str) -> Result<Package, ApiError> {
        if let Ok(purl) = PackageUrl::from_str(purl_str) {
            // get vulnerabilities from Guac
            let mut vulns = self
                .client
                .get_vulnerabilities(purl_str)
                .await
                .map_err(|_| ApiError::InternalError)?;

            // get vulnerabilities from Snyk
            let mut snyk_vulns = crate::snyk::get_vulnerabilities(self.snyk.clone(), purl_str)
                .await
                .map_err(|_| ApiError::InternalError)?;
            vulns.append(&mut snyk_vulns);

            //get related packages from Guac
            let trusted_versions: Vec<PackageRef> = self
                .client
                .get_packages(purl.clone())
                .await
                .map_err(|_| ApiError::InternalError)?;

            let p = Package {
                purl: Some(purl.to_string()),
                href: Some(format!(
                    "/api/package?purl={}",
                    &urlencoding::encode(&purl.to_string())
                )),
                trusted: Some(self.is_trusted(purl.clone())),
                trusted_versions,
                snyk: None,
                vulnerabilities: vulns,
                sbom: if self.sbom.exists(&purl.to_string()) {
                    Some(format!(
                        "/api/package/sbom?purl={}",
                        &urlencoding::encode(&purl.to_string())
                    ))
                } else {
                    None
                },
            };
            Ok(p)
        } else {
            Err(ApiError::InvalidPackageUrl {
                purl: purl_str.to_string(),
            })
        }
    }

    // temp fn to decide if the package is trusted based on its version or namespace
    fn is_trusted(&self, purl: PackageUrl<'_>) -> bool {
        purl.version().map_or(false, |v| v.contains("redhat"))
            || purl.namespace().map_or(false, |v| v == "redhat")
    }

    async fn get_all_trusted(&self) -> Result<Vec<Package>, ApiError> {
        let trusted_versions: Vec<Package> = self
            .client
            .get_all_packages()
            .await
            .map_err(|_| ApiError::InternalError)?;
        Ok(trusted_versions)
    }
}

#[utoipa::path(
    responses(
        (status = 200, description = "Package found", body = Package),
        (status = NOT_FOUND, description = "Package not found", body = Package, example = json!({
                "error": "Package pkg:rpm/redhat/openssl@1.1.1k-7.el8_9 was not found",
                "status": 404
        })),
        (status = BAD_REQUEST, description = "Invalid package URL"),
        (status = BAD_REQUEST, description = "Missing query argument")
    ),
    params(
        ("purl" = String, Query, description = "Package URL to query"),
    )
)]
#[get("/api/package")]
pub async fn get_package(
    data: web::Data<TrustedContent>,
    query: web::Query<PackageQuery>,
) -> Result<HttpResponse, ApiError> {
    if let Some(purl) = &query.purl {
        let p = data.get_trusted(purl).await?;
        Ok(HttpResponse::Ok().json(p))
    } else {
        Err(ApiError::MissingQueryArgument)
    }
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get the entire inventory", body = Vec<Package>),
    )
)]
#[get("/api/trusted")]
pub async fn get_trusted(data: web::Data<TrustedContent>) -> Result<HttpResponse, ApiError> {
    Ok(HttpResponse::Ok().json(data.get_all_trusted().await?))
}

#[utoipa::path(
    request_body = PackageList,
    responses(
        (status = 200, description = "Package found", body = Vec<Option<Package>>),
        (status = NOT_FOUND, description = "Package not found", body = Package, example = json!({
            "error": "Package pkg:rpm/redhat/openssl@1.1.1k-7.el8_9 was not found",
            "status": 404
    })),
        (status = BAD_REQUEST, description = "Invalid package URLs"),
    ),
)]
#[post("/api/package")]
pub async fn query_package(
    data: web::Data<TrustedContent>,
    body: Json<PackageList>,
) -> Result<HttpResponse, ApiError> {
    let mut packages: Vec<Option<Package>> = Vec::new();
    for purl in body.list().iter() {
        if let Ok(p) = data.get_trusted(purl).await {
            packages.push(Some(p));
        }
    }

    if packages.is_empty() {
        Err(ApiError::PackageNotFound {
            purl: body
                .list()
                .first()
                .ok_or(ApiError::MissingQueryArgument)?
                .to_string(),
        })
    } else {
        Ok(HttpResponse::Ok().json(packages))
    }
}

#[utoipa::path(
    request_body = PackageList,
    responses(
        (status = 200, description = "Package found", body = Vec<PackageDependencies>),
        (status = BAD_REQUEST, description = "Invalid package URL"),
    ),
)]
#[post("/api/package/dependencies")]
pub async fn query_package_dependencies(
    data: web::Data<Arc<Guac>>,
    body: Json<PackageList>,
) -> Result<HttpResponse, ApiError> {
    let mut dependencies: Vec<PackageDependencies> = Vec::new();
    for purl in body.list().iter() {
        if PackageUrl::from_str(purl).is_ok() {
            let lst = data
                .get_dependencies(purl)
                .await
                .map_err(|_| ApiError::InternalError)?;
            dependencies.push(lst);
        } else {
            return Err(ApiError::InvalidPackageUrl {
                purl: purl.to_string(),
            });
        }
    }
    Ok(HttpResponse::Ok().json(dependencies))
}

#[utoipa::path(
    request_body = PackageList,
    responses(
        (status = 200, description = "Package found", body = Vec<PackageDependents>),
        (status = BAD_REQUEST, description = "Invalid package URL"),
    ),
)]
#[post("/api/package/dependents")]
pub async fn query_package_dependents(
    data: web::Data<Arc<Guac>>,
    body: Json<PackageList>,
) -> Result<HttpResponse, ApiError> {
    let mut dependencies: Vec<PackageDependencies> = Vec::new();
    for purl in body.list().iter() {
        if PackageUrl::from_str(purl).is_ok() {
            let lst = data
                .get_dependents(purl)
                .await
                .map_err(|_| ApiError::InternalError)?;
            dependencies.push(lst);
        } else {
            return Err(ApiError::InvalidPackageUrl {
                purl: purl.to_string(),
            });
        }
    }
    Ok(HttpResponse::Ok().json(dependencies))
}

#[utoipa::path(
    request_body = PackageList,
    responses(
        (status = 200, description = "Package found", body = Vec<PackageRef>, example = json!(vec![
            (PackageRef {
                purl: "pkg:maven/io.vertx/vertx-web@4.3.4.redhat-00007".to_string(),
                href: format!("/api/package?purl={}", &urlencoding::encode("pkg:maven/io.vertx/vertx-web@4.3.4.redhat-00007")),
                trusted: Some(true),
                sbom: None,
                })]
        )),
        (status = BAD_REQUEST, description = "Invalid package URL"),
    ),
)]
#[post("/api/package/versions")]
pub async fn query_package_versions(
    data: web::Data<TrustedContent>,
    body: Json<PackageList>,
) -> Result<HttpResponse, ApiError> {
    let mut versions = Vec::new();
    for purl_str in body.list().iter() {
        if PackageUrl::from_str(purl_str).is_ok() {
            versions = data.get_versions(purl_str).await?;
        } else {
            return Err(ApiError::InvalidPackageUrl {
                purl: purl_str.to_string(),
            });
        }
    }
    Ok(HttpResponse::Ok().json(versions))
}

#[derive(serde::Deserialize)]
pub struct SBOMQuery {
    purl: Option<String>,
    #[serde(default)]
    download: bool,
}

#[utoipa::path(
    request_body = PackageList,
    responses(
        (status = 200, description = "SBOM found", body = serde_json::Value),
        (status = BAD_REQUEST, description = "Invalid package URL"),
    ),
)]
#[get("/api/package/sbom")]
pub async fn query_sbom(
    data: web::Data<Arc<SbomRegistry>>,
    query: web::Query<SBOMQuery>,
) -> Result<HttpResponse, ApiError> {
    if let Some(purl) = &query.purl {
        if let Some(value) = data.lookup(purl) {
            let mut response = HttpResponse::Ok();
            if query.download {
                response.append_header(ContentDisposition {
                    disposition: DispositionType::Attachment,
                    parameters: vec![
                        // TODO: I guess we can do better, but for now it's ok
                        DispositionParam::Filename("sbom.json".to_string()),
                    ],
                });
            }
            Ok(response.json(value))
        } else {
            Err(ApiError::PackageNotFound {
                purl: purl.to_string(),
            })
        }
    } else {
        Err(ApiError::MissingQueryArgument)
    }
}

#[derive(Debug, Error, Serialize, Deserialize)]
pub enum ApiError {
    #[error("No query argument was specified")]
    MissingQueryArgument,
    #[error("Package {purl} was not found")]
    PackageNotFound { purl: String },
    #[error("{purl} is not a valid package URL")]
    InvalidPackageUrl { purl: String },
    #[error("Error processing error internally")]
    InternalError,
}

impl error::ResponseError for ApiError {
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(serde_json::json!({
            "status": self.status_code().as_u16(),
            "error": self.to_string(),
        }))
    }

    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::MissingQueryArgument => StatusCode::BAD_REQUEST,
            ApiError::PackageNotFound { purl: _ } => StatusCode::NOT_FOUND,
            ApiError::InvalidPackageUrl { purl: _ } => StatusCode::BAD_REQUEST,
            ApiError::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
