use crate::genie::models::{GenieAction, GenieActionType, GenieResponse, UserIntent};
use regex::Regex;
use strsim::jaro_winkler;

pub struct GenieService;

impl GenieService {
    pub fn process_input(text: &str) -> GenieResponse {
        let intent = Self::parse_intent(text);

        match intent {
            UserIntent::AddBook(query) => GenieResponse {
                text: format!("I can help you add '{}' to your library. Shall I look it up?", query),
                actions: vec![GenieAction {
                    action_type: GenieActionType::AddBook,
                    payload: query.clone(),
                    label: format!("Search & Add '{}'", query),
                }],
            },
            UserIntent::SearchBook(query) => GenieResponse {
                text: format!("Searching your library for '{}'...", query),
                actions: vec![GenieAction {
                    action_type: GenieActionType::SearchBook,
                    payload: query,
                    label: "View Results".to_string(),
                }],
            },
            UserIntent::Unknown => GenieResponse {
                text: "I'm not sure what you mean. You can say 'Add Dune' or 'Search for Harry Potter'.".to_string(),
                actions: vec![],
            },
        }
    }

    fn parse_intent(text: &str) -> UserIntent {
        let text_lower = text.to_lowercase();

        // English: add, new, create
        // French: ajouter, ajoute, creer, créer, cree, crée, nouveau
        // Spanish: añadir, anadir, añade, anade, agregar, agrega, crear, crea, nuevo
        // German: hinzufügen, hinzufugen, füge hinzu, fuge hinzu, neu, erstellen, erstelle
        let add_pattern = Regex::new(r"(?i)^(add|new|create|insert|ajouter|ajoute|créer|creer|crée|cree|nouveau|añadir|anadir|anade|añade|agregar|agrega|crear|crea|nuevo|hinzufügen|hinzufugen|erstellen|erstelle)\s+(book\s+|livre\s+|libro\s+|buch\s+)?(.+)$").unwrap();

        // English: search, find, lookup, show, get
        // French: chercher, cherche, trouver, trouve, rechercher, recherche, voir
        // Spanish: buscar, busca, encontrar, encuentra, ver, mira
        // German: suchen, suche, finden, finde, zeigen, zeige
        let search_pattern = Regex::new(r"(?i)^(search|find|lookup|show|get|chercher|cherche|trouver|trouve|rechercher|recherche|voir|buscar|busca|encontrar|encuentra|ver|mira|suchen|suche|finden|finde|zeigen|zeige)\s+(for\s+|pour\s+|para\s+|nach\s+)?(.+)$").unwrap();

        if let Some(caps) = add_pattern.captures(&text_lower) {
            if let Some(query) = caps.get(3) {
                return UserIntent::AddBook(query.as_str().trim().to_string());
            }
        }

        if let Some(caps) = search_pattern.captures(&text_lower) {
            if let Some(query) = caps.get(3) {
                return UserIntent::SearchBook(query.as_str().trim().to_string());
            }
        }

        // Fallback: Fuzzy Logic (Simulated)
        // Check against common "add" verbs in supported languages
        let first_word = text_lower.split_whitespace().next().unwrap_or("");
        let add_keywords = ["add", "ajouter", "añadir", "agregar", "hinzufügen"];

        let is_add = add_keywords
            .iter()
            .any(|&keyword| jaro_winkler(first_word, keyword) > 0.85);

        if is_add {
            // Extract remainder
            let remainder = text_lower.trim_start_matches(first_word).trim();
            if !remainder.is_empty() {
                return UserIntent::AddBook(remainder.to_string());
            }
        }

        UserIntent::Unknown
    }
}
