use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum GenieActionType {
    AddBook,
    SearchBook,
    None,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GenieAction {
    pub action_type: GenieActionType,
    pub payload: String, // e.g., "Dune"
    pub label: String,   // e.g., "Add 'Dune'"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GenieResponse {
    pub text: String,
    pub actions: Vec<GenieAction>,
}

#[derive(Debug)]
pub enum UserIntent {
    AddBook(String),
    SearchBook(String),
    Unknown,
}
