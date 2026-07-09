#[cfg(test)]
mod tests {
    use candid_parser::{IDLProg, check_prog};
    use std::collections::BTreeSet;

    #[test]
    fn provision_did_parses_and_exposes_admin_method() {
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
            names.contains(&"admin_install_deployment_binding"),
            "admin_install_deployment_binding must be in the public ingress surface in this slice; got {:?}",
            names
        );
        for required in ["accept_envelope", "query_job", "router_ack"] {
            assert!(
                names.contains(&required),
                "missing method {} in provision.did; got {:?}",
                required,
                names
            );
        }

        // Regression guard: the method must return Result<BootstrapAuthEntry, AdminInstallError>,
        // not the earlier Result<Null, AdminInstallError> stub.
        let admin_method = methods
            .iter()
            .find(|(n, _)| n == "admin_install_deployment_binding")
            .map(|(_, ty)| ty.clone())
            .expect("admin_install_deployment_binding method type");
        let admin_rets = match admin_method.as_ref() {
            candid::types::TypeInner::Func(func) => &func.rets,
            _ => panic!("admin_install_deployment_binding must be a function"),
        };
        assert_eq!(
            admin_rets.len(),
            1,
            "admin_install_deployment_binding must return exactly one result variant"
        );
        let admin_ret = &admin_rets[0];
        let is_null_result = matches!(admin_ret.as_ref(), candid::types::TypeInner::Var(name) if {
            env.0.get(name).is_some_and(|ty| {
                if let candid::types::TypeInner::Variant(fields) = ty.as_ref() {
                    fields.iter().any(|f| {
                        f.id.to_string() == "Ok"
                            && matches!(f.ty.as_ref(), candid::types::TypeInner::Var(inner) if env
                                .0
                                .get(inner.as_str())
                                .is_some_and(|t| matches!(t.as_ref(), candid::types::TypeInner::Null)))
                    })
                } else {
                    false
                }
            })
        });
        assert!(
            !is_null_result,
            "admin_install_deployment_binding must not return Result<Null, AdminInstallError>"
        );

        for required in [
            "accept_envelope",
            "query_job",
            "router_ack",
            "admin_install_deployment_binding",
            "artifact_publish_metadata",
            "artifact_upload_chunk",
            "artifact_get_status",
            "release_publish",
            "release_activate",
            "release_get_active",
        ] {
            assert!(
                names.contains(&required),
                "missing method {} in provision.did; got {:?}",
                required,
                names
            );
        }

        for method_name in ["artifact_publish_metadata", "artifact_upload_chunk"] {
            let method = methods
                .iter()
                .find(|(n, _)| n == method_name)
                .map(|(_, ty)| ty.clone())
                .unwrap_or_else(|| panic!("{} method type", method_name));
            let rets = match method.as_ref() {
                candid::types::TypeInner::Func(func) => &func.rets,
                _ => panic!("{} must be a function", method_name),
            };
            assert_eq!(
                rets.len(),
                1,
                "{} must return exactly one variant",
                method_name
            );
            let ret = &rets[0];
            let is_null_result = matches!(ret.as_ref(), candid::types::TypeInner::Var(name) if {
                env.0.get(name).is_some_and(|ty| {
                    if let candid::types::TypeInner::Variant(fields) = ty.as_ref() {
                        fields.iter().any(|f| {
                            f.id.to_string() == "Ok"
                                && matches!(f.ty.as_ref(), candid::types::TypeInner::Var(inner) if env
                                    .0
                                    .get(inner.as_str())
                                    .is_some_and(|t| matches!(t.as_ref(), candid::types::TypeInner::Null)))
                        })
                    } else {
                        false
                    }
                })
            });
            assert!(
                !is_null_result,
                "{} must not return Result<Null, ArtifactError>",
                method_name
            );
        }

        for method_name in ["release_publish", "release_activate"] {
            let method = methods
                .iter()
                .find(|(n, _)| n == method_name)
                .map(|(_, ty)| ty.clone())
                .unwrap_or_else(|| panic!("{} method type", method_name));
            let rets = match method.as_ref() {
                candid::types::TypeInner::Func(func) => &func.rets,
                _ => panic!("{} must be a function", method_name),
            };
            assert_eq!(
                rets.len(),
                1,
                "{} must return exactly one variant",
                method_name
            );
            let ret = &rets[0];
            let is_null_result = matches!(ret.as_ref(), candid::types::TypeInner::Var(name) if {
                env.0.get(name).is_some_and(|ty| {
                    if let candid::types::TypeInner::Variant(fields) = ty.as_ref() {
                        fields.iter().any(|f| {
                            f.id.to_string() == "Ok"
                                && matches!(f.ty.as_ref(), candid::types::TypeInner::Var(inner) if env
                                    .0
                                    .get(inner.as_str())
                                    .is_some_and(|t| matches!(t.as_ref(), candid::types::TypeInner::Null)))
                        })
                    } else {
                        false
                    }
                })
            });
            assert!(
                !is_null_result,
                "{} must not return Result<Null, ReleaseError>",
                method_name
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
            "BootstrapAuthEntry",
            "BootstrapAuthAction",
            "BootstrapAuthorityRecord",
            "BootstrapAuthHistory",
            "AdminInstallDeploymentBindingArgs",
            "AdminInstallError",
            "CanisterKind",
            "ArtifactId",
            "ArtifactMetadata",
            "ArtifactUpload",
            "ArtifactChunkKey",
            "ArtifactChunk",
            "ArtifactUploadState",
            "ArtifactError",
            "ArtifactPublishMetadataArgs",
            "ArtifactUploadChunkArgs",
            "ReleaseId",
            "ReleaseManifest",
            "ReleaseActivateResult",
            "ReleaseError",
            "ReleasePublishArgs",
            "ReleaseActivateArgs",
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

        let generated_reachable = reachable_type_names(&generated_env, &generated_actor);
        let pruned_generated_env = candid::TypeEnv(
            generated_env
                .0
                .iter()
                .filter(|(name, _)| generated_reachable.contains(name.as_str()))
                .map(|(name, ty)| (name.clone(), ty.clone()))
                .collect(),
        );
        let generated_did =
            candid::pretty::candid::compile(&pruned_generated_env, &generated_actor);
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
