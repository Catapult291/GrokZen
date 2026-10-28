//! Protobuf (Protocol Buffers) post-parse hooks.
//!
//! Extraction is driven by `queries/proto/*.scm`; this module only holds
//! unit tests that pin the symbol / import shape we want agents to see.

#[cfg(test)]
mod tests {
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    const SAMPLE: &str = r#"
syntax = "proto3";
package demo.v1;

import "google/protobuf/timestamp.proto";
import "common/types.proto";

option java_package = "com.demo.v1";

message User {
  string id = 1;
  google.protobuf.Timestamp created_at = 2;
  enum Role {
    ROLE_UNSPECIFIED = 0;
    ROLE_ADMIN = 1;
  }
  Role role = 3;
}

enum Status {
  STATUS_UNSPECIFIED = 0;
  STATUS_ACTIVE = 1;
}

service UserService {
  rpc GetUser (GetUserRequest) returns (User);
  rpc ListUsers (ListUsersRequest) returns (stream User);
}

message GetUserRequest {
  string id = 1;
}
"#;

    #[test]
    fn extracts_package_message_enum_service_rpc_and_imports() {
        let pool = ParserPool::new();
        let file = pool
            .parse(Language::Proto, &RelPath::new("demo/v1/user.proto"), SAMPLE)
            .expect("parse");
        let names: Vec<&str> = file.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.iter().any(|n| *n == "demo.v1" || n.contains("demo")),
            "package symbol missing: {names:?}"
        );
        assert!(names.contains(&"User"), "message User: {names:?}");
        assert!(names.contains(&"Status"), "enum Status: {names:?}");
        assert!(names.contains(&"UserService"), "service: {names:?}");
        assert!(names.contains(&"GetUser"), "rpc GetUser: {names:?}");
        assert!(names.contains(&"ListUsers"), "rpc ListUsers: {names:?}");
        // Nested enum Role should appear
        assert!(names.contains(&"Role"), "nested enum Role: {names:?}");

        let kinds: Vec<SymbolKind> = file.symbols.iter().map(|s| s.kind).collect();
        assert!(
            kinds.contains(&SymbolKind::Class),
            "message User should be Class: {kinds:?}"
        );
        assert!(
            kinds.contains(&SymbolKind::Enum),
            "enum Status should be Enum: {kinds:?}"
        );
        assert!(
            kinds.contains(&SymbolKind::Interface),
            "service should be Interface: {kinds:?}"
        );
        assert!(
            kinds.contains(&SymbolKind::Method),
            "rpc should be Method: {kinds:?}"
        );
        assert!(
            kinds.contains(&SymbolKind::Module),
            "package should be Module: {kinds:?}"
        );

        assert!(
            file.imports
                .iter()
                .any(|t| t.contains("timestamp.proto") || t.contains("google/protobuf")),
            "imports: {:?}",
            file.imports
        );
        assert!(
            file.imports
                .iter()
                .any(|t| t.contains("common/types.proto") || t.contains("types.proto")),
            "imports: {:?}",
            file.imports
        );
    }

    #[test]
    fn from_path_maps_proto_extension() {
        assert_eq!(
            Language::from_path(&RelPath::new("api/foo.proto")),
            Some(Language::Proto)
        );
    }
}
