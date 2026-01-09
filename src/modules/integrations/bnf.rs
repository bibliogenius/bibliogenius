//! BNF (Bibliothèque nationale de France) integration via data.bnf.fr SPARQL endpoint
//!
//! This module provides search functionality for French books via the BNF's
//! Linked Open Data SPARQL endpoint.

use serde::{Deserialize, Serialize};

/// A book result from BNF search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BnfBook {
    pub title: String,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub bnf_uri: String,
    pub description: Option<String>,
}

/// SPARQL response structures
#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    work: Option<SparqlValue>,
    title: Option<SparqlValue>,
    #[serde(rename = "author")]
    _author: Option<SparqlValue>,
    #[serde(rename = "authorName")]
    author_name: Option<SparqlValue>,
    publisher: Option<SparqlValue>,
    date: Option<SparqlValue>,
    isbn: Option<SparqlValue>,
    description: Option<SparqlValue>,
}

#[derive(Debug, Deserialize)]
struct SparqlValue {
    value: String,
}

/// Search for books on data.bnf.fr using SPARQL
///
/// # Arguments
/// * `query` - Search query (title, author, or general search)
///
/// # Returns
/// A vector of BnfBook results
pub async fn search_bnf(query: &str) -> Result<Vec<BnfBook>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // SPARQL query to search for books by title or author
    // Using IFLA LRM model: Work -> Expression -> Manifestation
    let sparql_query = format!(
        r#"
PREFIX dcterms: <http://purl.org/dc/terms/>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX bnf-onto: <http://data.bnf.fr/ontology/bnf-onto/>
PREFIX rdarelationships: <http://rdvocab.info/RDARelationshipsWEMI/>

SELECT DISTINCT ?work ?title ?authorName ?publisher ?date ?isbn ?description
WHERE {{
    ?work a <http://data.bnf.fr/ontology/bnf-onto/Work> ;
          dcterms:title ?title .
    
    OPTIONAL {{
        ?work dcterms:creator ?author .
        ?author foaf:name ?authorName .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation dcterms:publisher ?publisherEntity .
        ?publisherEntity foaf:name ?publisher .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation dcterms:date ?date .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation bnf-onto:isbn ?isbn .
    }}
    
    OPTIONAL {{
        ?work dcterms:description ?description .
    }}
    
    FILTER(
        CONTAINS(LCASE(?title), LCASE("{search}")) ||
        CONTAINS(LCASE(COALESCE(?authorName, "")), LCASE("{search}"))
    )
}}
LIMIT 20
"#,
        search = query.replace('"', r#"\""#)
    );

    let response = client
        .get("https://data.bnf.fr/sparql")
        .query(&[
            ("query", sparql_query.as_str()),
            ("format", "application/sparql-results+json"),
        ])
        .send()
        .await
        .map_err(|e| format!("BNF API request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "BNF API returned error status: {}",
            response.status()
        ));
    }

    let sparql_result: SparqlResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse BNF response: {}", e))?;

    let mut books: Vec<BnfBook> = Vec::new();
    let mut seen_uris: std::collections::HashSet<String> = std::collections::HashSet::new();

    for binding in sparql_result.results.bindings {
        let uri = binding
            .work
            .as_ref()
            .map(|w| w.value.clone())
            .unwrap_or_default();

        // Deduplicate by work URI
        if seen_uris.contains(&uri) {
            continue;
        }
        seen_uris.insert(uri.clone());

        let title = binding
            .title
            .as_ref()
            .map(|t| t.value.clone())
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }

        // Parse year from date string (could be "2020", "2020-01-01", etc.)
        let year = binding.date.as_ref().and_then(|d| {
            d.value
                .split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
        });

        // Generate cover URL from BNF if available
        // BNF provides covers via their Gallica service for some works
        let cover_url = generate_bnf_cover_url(&uri);

        let book = BnfBook {
            title,
            author: binding.author_name.map(|a| a.value),
            publisher: binding.publisher.map(|p| p.value),
            publication_year: year,
            isbn: binding.isbn.map(|i| i.value),
            cover_url,
            bnf_uri: uri,
            description: binding.description.map(|d| d.value),
        };

        books.push(book);
    }

    Ok(books)
}

/// Generate a potential cover URL from BNF
/// BNF doesn't always have covers, but we can try the Gallica thumbnail service
fn generate_bnf_cover_url(bnf_uri: &str) -> Option<String> {
    // Extract the ARK identifier from the URI
    // URI format: http://data.bnf.fr/ark:/12148/cb123456789
    if let Some(ark_start) = bnf_uri.find("ark:/") {
        let ark = &bnf_uri[ark_start..];
        // Try Gallica thumbnail service
        return Some(format!(
            "https://gallica.bnf.fr/{}/thumbnail",
            ark.replace("ark:/", "ark:")
        ));
    }
    None
}

/// Search for a book by ISBN on data.bnf.fr
pub async fn lookup_bnf_isbn(isbn: &str) -> Result<Option<BnfBook>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // SPARQL query to search for exact ISBN
    let sparql_query = format!(
        r#"
PREFIX dcterms: <http://purl.org/dc/terms/>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX bnf-onto: <http://data.bnf.fr/ontology/bnf-onto/>
PREFIX rdarelationships: <http://rdvocab.info/RDARelationshipsWEMI/>

SELECT DISTINCT ?work ?title ?authorName ?publisher ?date ?isbn ?description
WHERE {{
    ?manifestation bnf-onto:isbn "{isbn}" .
    ?work rdarelationships:expressionManifested ?manifestation ;
          dcterms:title ?title .
    
    OPTIONAL {{
        ?work dcterms:creator ?author .
        ?author foaf:name ?authorName .
    }}
    
    OPTIONAL {{
        ?manifestation dcterms:publisher ?publisherEntity .
        ?publisherEntity foaf:name ?publisher .
    }}
    
    OPTIONAL {{
        ?manifestation dcterms:date ?date .
    }}
    
    OPTIONAL {{
        ?work dcterms:description ?description .
    }}
}}
LIMIT 1
"#,
        isbn = isbn.replace('-', "")
    );

    let response = client
        .get("https://data.bnf.fr/sparql")
        .query(&[
            ("query", sparql_query.as_str()),
            ("format", "application/sparql-results+json"),
        ])
        .send()
        .await
        .map_err(|e| format!("BNF API request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "BNF API returned error status: {}",
            response.status()
        ));
    }

    let sparql_result: SparqlResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse BNF response: {}", e))?;

    if let Some(binding) = sparql_result.results.bindings.first() {
        let uri = binding
            .work
            .as_ref()
            .map(|w| w.value.clone())
            .unwrap_or_default();

        let title = binding
            .title
            .as_ref()
            .map(|t| t.value.clone())
            .unwrap_or_default();

        // Parse year from date string
        let year = binding.date.as_ref().and_then(|d| {
            d.value
                .split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
        });

        let cover_url = generate_bnf_cover_url(&uri);

        Ok(Some(BnfBook {
            title,
            author: binding.author_name.as_ref().map(|a| a.value.clone()),
            publisher: binding.publisher.as_ref().map(|p| p.value.clone()),
            publication_year: year,
            isbn: Some(isbn.to_string()),
            cover_url,
            bnf_uri: uri,
            description: binding.description.as_ref().map(|d| d.value.clone()),
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_search_bnf() {
        let results = search_bnf("Victor Hugo").await;
        assert!(results.is_ok());
    }
}
