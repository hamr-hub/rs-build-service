use std::collections::HashMap;

use uuid::Uuid;

use crate::Note;

#[derive(Default)]
pub struct MemStore {
    notes: HashMap<Uuid, Note>,
}

impl MemStore {
    pub fn insert(&mut self, note: Note) {
        self.notes.insert(note.id, note);
    }

    pub fn get(&self, id: Uuid) -> Option<Note> {
        self.notes.get(&id).cloned()
    }

    pub fn list(&self) -> Vec<Note> {
        let mut notes: Vec<_> = self.notes.values().cloned().collect();
        notes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        notes
    }
}
