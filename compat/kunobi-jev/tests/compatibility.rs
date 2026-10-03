#[test]
fn legacy_imports_share_the_new_types_and_provider_api() {
    let mut questions = kunobi_jev::Questions::new();
    questions.add("ready", kunobi_jev::noul("Ready?"));
    let _: kunobi_decision::SystemOneRequest =
        kunobi_jev::SystemOneRequest::new("ready", questions);
    assert_eq!(kunobi_jev::Provider::Liquid.default_model(), "d1:free");
}
