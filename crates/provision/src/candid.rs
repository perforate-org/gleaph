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
            names.contains(&"upsert_deployment_grant"),
            "upsert_deployment_grant must be in the public ingress surface in this slice; got {:?}",
            names
        );
        for required in [
            "accept_envelope",
            "query_job",
            "complete_graph_registration",
        ] {
            assert!(
                names.contains(&required),
                "missing method {} in provision.did; got {:?}",
                required,
                names
            );
        }

        // Regression guard: the method must return Result<BootstrapAuthEntry, UpsertDeploymentGrantError>,
        // not the earlier Result<Null, UpsertDeploymentGrantError> stub.
        let admin_method = methods
            .iter()
            .find(|(n, _)| n == "upsert_deployment_grant")
            .map(|(_, ty)| ty.clone())
            .expect("upsert_deployment_grant method type");
        let admin_rets = match admin_method.as_ref() {
            candid::types::TypeInner::Func(func) => &func.rets,
            _ => panic!("upsert_deployment_grant must be a function"),
        };
        assert_eq!(
            admin_rets.len(),
            1,
            "upsert_deployment_grant must return exactly one result variant"
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
            "upsert_deployment_grant must not return Result<Null, UpsertDeploymentGrantError>"
        );

        for required in [
            "accept_envelope",
            "query_job",
            "complete_graph_registration",
            "upsert_deployment_grant",
            "artifact_publish_metadata",
            "artifact_upload_chunk",
            "artifact_get_status",
            "release_publish",
            "release_activate",
            "release_get_active",
            "release_install",
            "artifact_audit_history",
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

        for method_name in ["release_publish", "release_activate", "release_install"] {
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
                "{} must not return Result<Null, Error>",
                method_name
            );
        }

        let declared_types: Vec<&str> = env.0.keys().map(|name| name.as_str()).collect();
        for required_type in [
            "ProvisioningIntentKey",
            "ProvisionIngressError",
            "ProvisionInitArgs",
            "ProvisionIngressResult",
            "RouterRegistrationAck",
            "RouterRegistrationAckResponse",
            "RouterRegistrationAckResult",
            "BootstrapAuthEntry",
            "BootstrapAuthAction",
            "UpsertDeploymentGrantArgs",
            "UpsertDeploymentGrantError",
            "CanisterKind",
            "ArtifactId",
            "ArtifactMetadata",
            "ArtifactUpload",
            "ArtifactUploadState",
            "ArtifactError",
            "ArtifactPublishMetadataArgs",
            "ArtifactUploadChunkArgs",
            "ReleaseManifest",
            "ReleaseActivateResult",
            "ReleaseError",
            "ReleasePublishArgs",
            "ReleaseActivateArgs",
            "ArtifactAuditEntry",
            "ArtifactAuditAction",
            "ArtifactAuditOutcome",
            "ReleaseInstallArgs",
            "ReleaseInstallResult",
            "InstallError",
            "Result_5",
            "Result_6",
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

    #[test]
    fn registration_ack_service_shape_is_exact() {
        use candid::types::{Type, TypeInner};

        fn resolve<'a>(env: &'a candid::TypeEnv, ty: &'a Type) -> &'a Type {
            let mut current = ty;
            while let TypeInner::Var(name) = current.as_ref() {
                current = env.0.get(name).expect("referenced Candid type");
            }
            current
        }

        fn labels(env: &candid::TypeEnv, ty: &Type) -> Vec<String> {
            let mut labels = match resolve(env, ty).as_ref() {
                TypeInner::Record(fields) | TypeInner::Variant(fields) => fields
                    .iter()
                    .map(|field| field.id.to_string())
                    .collect::<Vec<_>>(),
                other => panic!("expected record or variant, got {other:?}"),
            };
            labels.sort();
            labels
        }

        let generated = crate::export_service_string();
        let ast: IDLProg = generated.parse().expect("generated Candid parses");
        let mut env = candid::TypeEnv::new();
        let actor = check_prog(&mut env, &ast)
            .expect("generated Candid checks")
            .expect("generated service actor");
        let methods = env.as_service(&actor).expect("Provision service");
        let names: Vec<&str> = methods.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"complete_graph_registration"));
        assert!(!names.contains(&"router_ack"));

        let method = methods
            .iter()
            .find(|(name, _)| name == "complete_graph_registration")
            .map(|(_, ty)| ty)
            .expect("registration completion method");
        let function = match resolve(&env, method).as_ref() {
            TypeInner::Func(function) => function,
            other => panic!("expected function, got {other:?}"),
        };
        assert_eq!(function.args.len(), 1);
        assert_eq!(function.rets.len(), 1);
        assert_eq!(
            labels(&env, &function.args[0]),
            ["deployment_id", "request_id"]
        );
        assert_eq!(labels(&env, &function.rets[0]), ["Err", "Ok"]);

        let result = resolve(&env, &function.rets[0]);
        let ok_type = match result.as_ref() {
            TypeInner::Variant(fields) => {
                &fields
                    .iter()
                    .find(|field| field.id.to_string() == "Ok")
                    .expect("Ok variant")
                    .ty
            }
            other => panic!("expected result variant, got {other:?}"),
        };
        assert_eq!(labels(&env, ok_type), ["Applied", "Replay"]);
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
