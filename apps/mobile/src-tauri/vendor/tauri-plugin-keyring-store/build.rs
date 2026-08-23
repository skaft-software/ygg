const COMMANDS: &[&str] = &[
    "get_passwords",
    "set_passwords",
    "delete_passwords",
    "password_exists",
    "export_passwords_plain",
    "import_passwords_plain",
    "export_passwords_encrypted",
    "import_passwords_encrypted",
    "ping",
    "initialize",
    "destroy",
    "save",
    "create_client",
    "load_client",
    "get_store_record",
    "save_store_record",
    "remove_store_record",
    "save_secret",
    "remove_secret",
    "execute_procedure",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
