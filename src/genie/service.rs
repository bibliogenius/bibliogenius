use regex::Regex;
use strsim::jaro_winkler;
use crate::genie::models::{GenieResponse, GenieAction, GenieActionType, UserIntent};

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
        
        // Regex Patterns
        let add_pattern = Regex::new(r"(?i)^(add|new|create)\s+(book\s+)?(.+)$").unwrap();
        let search_pattern = Regex::new(r"(?i)^(search|find|lookup|show)\s+(for\s+)?(.+)$").unwrap();

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
        // If it starts with something close to "add"
        let first_word = text_lower.split_whitespace().next().unwrap_or("");
        if jaro_winkler(first_word, "add") > 0.85 {
             // Extract remainder
             let remainder = text_lower.trim_start_matches(first_word).trim();
             if !remainder.is_empty() {
                 return UserIntent::AddBook(remainder.to_string());
             }
        }

        UserIntent::Unknown
    }
}
