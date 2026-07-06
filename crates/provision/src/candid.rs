#[cfg(test)]
mod tests {
    use candid_parser::{IDLProg, check_prog};

    #[test]
    fn provision_did_parses_and_exposes_three_methods() {
        let did = include_str!("../provision.did");
        let ast: IDLProg = did
            .parse()
            .expect("provision.did must parse as a Candid program");
        let mut env = candid::TypeEnv::new();
        let actor = check_prog(&mut env, &ast)
            .expect("provision.did must be a valid Candid program")
            .expect("provision.did must declare a service");
        let methods = env
            .as_service(&actor)
            .expect("actor must be a Candid service");
        let names: Vec<&str> = methods.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"admin_install_deployment_binding"),
            "admin_install_deployment_binding must not be in the public ingress surface in this slice"
        );
        for required in ["accept_envelope", "query_job", "router_ack"] {
            assert!(
                names.contains(&required),
                "missing method {} in provision.did; got {:?}",
                required,
                names
            );
        }
    }
}
