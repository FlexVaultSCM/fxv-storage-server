use error_chain::error_chain;

error_chain! {
    foreign_links {
        Io(std::io::Error);
        StripPrefix(std::path::StripPrefixError);
    }

    errors {
        PathTraversal {
            description("path traversal attempt detected")
        }
        InvalidRange(msg: String) {
            description("invalid range header")
            display("invalid range: {}", msg)
        }
        UploadNotFound(id: String) {
            description("multipart upload not found")
            display("upload not found: {}", id)
        }
        PartNotFound(upload_id: String, part_number: u32) {
            description("multipart part not found")
            display("part {} not found for upload {}", part_number, upload_id)
        }
    }
}
