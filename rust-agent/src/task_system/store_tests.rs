#[cfg(test)]
mod tests {
    use super::super::TaskStoreError;
    use std::io;
    use serde_json;

    #[test]
    fn test_invalid_id_error() {
        let error = TaskStoreError::InvalidId("invalid-id".to_string());
        assert_eq!(error.to_string(), "Invalid task ID: invalid-id");
    }

    #[test]
    fn test_not_found_error() {
        let error = TaskStoreError::NotFound("task-123".to_string());
        assert_eq!(error.to_string(), "Task not found: task-123");
    }

    #[test]
    fn test_escapes_workspace_error() {
        let error = TaskStoreError::EscapesWorkspace;
        assert_eq!(error.to_string(), "Task store escapes workspace");
    }

    #[test]
    fn test_io_error() {
        let io_error = io::Error::new(io::ErrorKind::NotFound, "File not found");
        let error = TaskStoreError::Io(io_error);
        assert_eq!(error.to_string(), "IO error: File not found");
    }

    #[test]
    fn test_json_error() {
        let io_error = io::Error::new(io::ErrorKind::InvalidData, "Invalid JSON");
        let json_error = serde_json::Error::io(io_error);
        let error = TaskStoreError::Json(json_error);
        assert_eq!(error.to_string(), "JSON error: Invalid JSON");
    }

    #[test]
    fn test_invalid_status_error() {
        let error = TaskStoreError::InvalidStatus("invalid_status".to_string());
        assert_eq!(error.to_string(), "Invalid task status: invalid_status");
    }
}