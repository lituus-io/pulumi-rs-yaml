/// Owns the source text for all YAML files in a program.
///
/// All `&'src str` references in the AST and evaluation layers borrow from a `SourceArena`.
/// This enforces the lifetime invariant: source outlives AST outlives evaluation.
pub struct SourceArena {
    files: Vec<SourceFile>,
}

/// A single source file with its name and contents.
pub struct SourceFile {
    name: String,
    text: String,
}

/// Index into `SourceArena::files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

impl SourceArena {
    /// Creates a new, empty source arena.
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Adds a file to the arena and returns its `FileId`.
    pub fn add_file(&mut self, name: String, text: String) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile { name, text });
        id
    }

    /// Returns the source text for the given file.
    pub fn text(&self, id: FileId) -> &str {
        &self.files[id.0 as usize].text
    }

    /// Returns the file name for the given file.
    pub fn name(&self, id: FileId) -> &str {
        &self.files[id.0 as usize].name
    }

    /// Returns the number of files in the arena.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Returns an iterator over all file IDs.
    pub fn file_ids(&self) -> impl Iterator<Item = FileId> {
        (0..self.files.len() as u32).map(FileId)
    }
}

impl Default for SourceArena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_retrieve_file() {
        let mut arena = SourceArena::new();
        let id = arena.add_file("Pulumi.yaml".to_string(), "name: test\n".to_string());
        assert_eq!(arena.name(id), "Pulumi.yaml");
        assert_eq!(arena.text(id), "name: test\n");
    }

    #[test]
    fn test_multiple_files() {
        let mut arena = SourceArena::new();
        let id0 = arena.add_file("a.yaml".to_string(), "a: 1\n".to_string());
        let id1 = arena.add_file("b.yaml".to_string(), "b: 2\n".to_string());
        assert_eq!(arena.text(id0), "a: 1\n");
        assert_eq!(arena.text(id1), "b: 2\n");
        assert_eq!(arena.file_count(), 2);
    }

    #[test]
    fn test_borrow_lifetime() {
        let mut arena = SourceArena::new();
        let id = arena.add_file("test.yaml".to_string(), "hello world".to_string());
        // This tests that we can borrow the text for 'src lifetime
        let text: &str = arena.text(id);
        assert!(text.contains("hello"));
    }

    #[test]
    fn test_file_ids_iterator() {
        let mut arena = SourceArena::new();
        arena.add_file("a.yaml".to_string(), String::new());
        arena.add_file("b.yaml".to_string(), String::new());
        arena.add_file("c.yaml".to_string(), String::new());
        let ids: Vec<_> = arena.file_ids().collect();
        assert_eq!(ids, vec![FileId(0), FileId(1), FileId(2)]);
    }

    #[test]
    fn test_empty_arena() {
        let arena = SourceArena::new();
        assert_eq!(arena.file_count(), 0);
        assert_eq!(arena.file_ids().count(), 0);
    }
}
