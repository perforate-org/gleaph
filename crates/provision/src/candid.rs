#[cfg(test)]
mod tests {
    use candid_parser::{IDLProg, check_prog};
    use std::collections::BTreeSet;

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

        let declared_types: Vec<&str> = env.0.keys().map(|name| name.as_str()).collect();
        for required_type in [
            "CreatedResource",
            "ProvisionResult",
            "ProvisionResultOutcome",
            "ProvisioningIntentKey",
            "ProvisionIngressError",
            "ProvisionInitArgs",
            "ProvisionIngressResult",
            "RouterAckResult",
        ] {
            assert!(
                declared_types.contains(&required_type),
                "missing type {} in provision.did; got {:?}",
                required_type,
                declared_types
            );
        }
    }

    #[test]
    fn test_provision_did_export_service_matches_handwritten() {
        let generated = crate::export_service_string();
        let generated_ast: IDLProg = generated
            .parse()
            .expect("generated candid must parse as a Candid program");
        let mut generated_env = candid::TypeEnv::new();
        let generated_actor = check_prog(&mut generated_env, &generated_ast)
            .expect("generated candid must be a valid Candid program");

        let handwritten = include_str!("../provision.did");
        let handwritten_ast: IDLProg = handwritten
            .parse()
            .expect("hand-written provision.did must parse as a Candid program");
        let mut handwritten_env = candid::TypeEnv::new();
        let handwritten_actor = check_prog(&mut handwritten_env, &handwritten_ast)
            .expect("hand-written provision.did must be a valid Candid program");

        let reachable = reachable_type_names(&handwritten_env, &handwritten_actor);
        let pruned_env = candid::TypeEnv(
            handwritten_env
                .0
                .iter()
                .filter(|(name, _)| reachable.contains(name.as_str()))
                .map(|(name, ty)| (name.clone(), ty.clone()))
                .collect(),
        );

        let generated_did = candid::pretty::candid::compile(&generated_env, &generated_actor);
        let handwritten_did = candid::pretty::candid::compile(&pruned_env, &handwritten_actor);
        assert_eq!(
            generated_did, handwritten_did,
            "generated service (after normalization) must match hand-written provision.did"
        );
    }

    fn reachable_type_names(
        env: &candid::TypeEnv,
        actor: &Option<candid::types::Type>,
    ) -> BTreeSet<String> {
        use candid::types::{Field, Type, TypeInner};
        let mut reachable = BTreeSet::new();
        let mut queue: Vec<Type> = Vec::new();
        if let Some(ty) = actor {
            queue.push(ty.clone());
        }
        while let Some(ty) = queue.pop() {
            match ty.as_ref() {
                TypeInner::Var(name) => {
                    if reachable.insert(name.clone())
                        && let Some(def) = env.0.get(name)
                    {
                        queue.push(def.clone());
                    }
                }
                TypeInner::Opt(inner) | TypeInner::Vec(inner) => queue.push(inner.clone()),
                TypeInner::Record(fields) | TypeInner::Variant(fields) => {
                    for Field { ty, .. } in fields {
                        queue.push(ty.clone());
                    }
                }
                TypeInner::Func(func) => {
                    for t in func.args.iter().chain(func.rets.iter()) {
                        queue.push(t.clone());
                    }
                }
                TypeInner::Service(methods) => {
                    for (_, t) in methods {
                        queue.push(t.clone());
                    }
                }
                TypeInner::Class(init_args, service) => {
                    for t in init_args {
                        queue.push(t.clone());
                    }
                    queue.push(service.clone());
                }
                _ => {}
            }
        }
        reachable
    }
}
