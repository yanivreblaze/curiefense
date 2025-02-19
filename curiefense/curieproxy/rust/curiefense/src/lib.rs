pub mod acl;
pub mod body;
pub mod config;
pub mod contentfilter;
pub mod flow;
pub mod interface;
pub mod limit;
pub mod logs;
pub mod maxmind;
pub mod redis;
pub mod requestfields;
pub mod securitypolicy;
pub mod tagging;
pub mod utils;

use serde_json::json;

use acl::{check_acl, AclDecision, AclResult, BotHuman};
use config::{with_config, HSDB};
use contentfilter::{content_filter_check, masking};
use flow::flow_check;
use interface::Tags;
use interface::{challenge_phase01, challenge_phase02, Action, ActionType, Decision, Grasshopper, SimpleDecision};
use limit::limit_check;
use logs::Logs;
use securitypolicy::match_securitypolicy;
use tagging::tag_request;
use utils::{map_request, BodyDecodingResult, RawRequest, RequestInfo};

fn acl_block(blocking: bool, code: i32, tags: &[String]) -> Decision {
    Decision::Action(Action {
        atype: if blocking {
            ActionType::Block
        } else {
            ActionType::Monitor
        },
        block_mode: blocking,
        ban: false,
        status: 403,
        headers: None,
        reason: json!({"action": code, "initiator": "acl", "reason": tags }),
        content: "access denied".to_string(),
        extra_tags: None,
    })
}

fn challenge_verified<GH: Grasshopper>(gh: &GH, reqinfo: &RequestInfo, logs: &mut Logs) -> bool {
    if let Some(rbzid) = reqinfo.cookies.get("rbzid") {
        if let Some(ua) = reqinfo.headers.get("user-agent") {
            logs.debug(format!("Checking rbzid cookie {} with user-agent {}", rbzid, ua));
            return match gh.parse_rbzid(&rbzid.replace('-', "="), ua) {
                Some(b) => b,
                None => {
                    logs.error("Something when wrong when calling parse_rbzid");
                    false
                }
            };
        } else {
            logs.warning("Could not find useragent!");
        }
    } else {
        logs.warning("Could not find rbzid cookie!")
    }
    false
}

// generic entry point when the request map has already been parsed
pub fn inspect_generic_request_map<GH: Grasshopper>(
    configpath: &str,
    mgh: Option<GH>,
    raw: RawRequest,
    itags: Tags,
    logs: &mut Logs,
) -> (Decision, Tags, RequestInfo) {
    let mut tags = itags;

    // insert the all tag here, to make sure it is always present, even in the presence of early errors
    tags.insert("all");

    logs.debug(format!("Inspection starts (grasshopper active: {})", mgh.is_some()));

    // do all config queries in the lambda once
    // there is a lot of copying taking place, to minimize the lock time
    // this decision should be backed with benchmarks

    let ((nm, securitypolicy), (ntags, globalfilter_dec), flows, reqinfo, is_human) =
        match with_config(configpath, logs, |slogs, cfg| {
            let mmapinfo =
                match_securitypolicy(&raw.get_host(), &raw.meta.path, cfg, slogs).map(|(nm, um)| (nm, um.clone()));
            match mmapinfo {
                Some((nm, secpolicy)) => {
                    let reqinfo = map_request(
                        slogs,
                        &secpolicy.content_filter_profile.decoding,
                        &secpolicy.content_filter_profile.content_type,
                        &raw,
                    );
                    let nflows = cfg.flows.clone();

                    // without grasshopper, default to being human
                    let is_human = if let Some(gh) = &mgh {
                        challenge_verified(gh, &reqinfo, slogs)
                    } else {
                        false
                    };

                    let ntags = tag_request(is_human, cfg, &reqinfo);
                    Some(((nm, secpolicy), ntags, nflows, reqinfo, is_human))
                }
                None => {
                    slogs.error("Could not find a security policy");
                    None
                }
            }
        }) {
            Some(Some(x)) => x,
            Some(None) => {
                logs.debug("Something went wrong during request tagging");
                return (Decision::Pass, tags, map_request(logs, &[], &[], &raw));
            }
            None => {
                logs.debug("Something went wrong during security policy searching");
                return (Decision::Pass, tags, map_request(logs, &[], &[], &raw));
            }
        };
    let masking_seed = &securitypolicy.content_filter_profile.masking_seed;

    logs.debug("request tagged");
    tags.extend(ntags);
    tags.insert_qualified("securitypolicy", &nm);
    tags.insert_qualified("securitypolicy-entry", &securitypolicy.name);
    tags.insert_qualified("aclid", &securitypolicy.acl_profile.id);
    tags.insert_qualified("aclname", &securitypolicy.acl_profile.name);
    tags.insert_qualified("contentfilterid", &securitypolicy.content_filter_profile.id);
    tags.insert_qualified("contentfiltername", &securitypolicy.content_filter_profile.name);

    if !securitypolicy.content_filter_profile.content_type.is_empty()
        && reqinfo.rinfo.qinfo.body_decoding != BodyDecodingResult::ProperlyDecoded
    {
        let error: &str = if let BodyDecodingResult::DecodingFailed(rr) = &reqinfo.rinfo.qinfo.body_decoding {
            rr
        } else {
            "Expected a body, but there were none"
        };
        // we expect the body to be properly decoded
        let action = Action {
            reason: json!({
                "initiator": "body_decoding",
                "error": error
            }),
            status: 403,
            ..Action::default()
        };
        return (
            Decision::Action(action),
            tags,
            masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
        );
    }

    if let Some(dec) = mgh
        .as_ref()
        .and_then(|gh| challenge_phase02(gh, &reqinfo.rinfo.qinfo.uri, &reqinfo.headers))
    {
        return (
            dec,
            tags,
            masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
        );
    }
    logs.debug("challenge phase2 ignored");

    if let SimpleDecision::Action(action, reason) = globalfilter_dec {
        logs.debug(format!("Global filter decision {:?}", reason));
        let decision = action.to_decision(is_human, &mgh, &reqinfo.headers, reason);
        if decision.is_final() {
            return (
                decision,
                tags,
                masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
            );
        }
    }

    match flow_check(logs, &flows, &reqinfo, &mut tags) {
        Err(rr) => logs.error(rr),
        Ok(SimpleDecision::Pass) => {}
        Ok(SimpleDecision::Action(a, reason)) => {
            let decision = a.to_decision(is_human, &mgh, &reqinfo.headers, reason);
            if decision.is_final() {
                return (
                    decision,
                    tags,
                    masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
                );
            }
        }
    }
    logs.debug("flow checks done");

    // limit checks
    let limit_check = limit_check(logs, &securitypolicy.name, &reqinfo, &securitypolicy.limits, &mut tags);
    if let SimpleDecision::Action(action, reason) = limit_check {
        let decision = action.to_decision(is_human, &mgh, &reqinfo.headers, reason);
        if decision.is_final() {
            return (
                decision,
                tags,
                masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
            );
        }
    }
    logs.debug(format!("limit checks done ({} limits)", securitypolicy.limits.len()));

    let acl_result = check_acl(&tags, &securitypolicy.acl_profile);
    logs.debug(format!("ACL result: {:?}", acl_result));
    // store the check_acl result here
    let blockcode: Option<(i32, Vec<String>)> = match acl_result {
        AclResult::Passthrough(dec) => {
            if dec.allowed {
                logs.debug("ACL passthrough detected");
                return (
                    Decision::Pass,
                    tags,
                    masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
                );
            } else {
                logs.debug("ACL force block detected");
                Some((0, dec.tags))
            }
        }
        // bot blocked, human blocked
        // effect would be identical to the following case except for logging purpose
        AclResult::Match(BotHuman {
            bot: Some(AclDecision {
                allowed: false,
                tags: bot_tags,
            }),
            human: Some(AclDecision {
                allowed: false,
                tags: human_tags,
            }),
        }) => {
            logs.debug("ACL human block detected");
            Some((5, if is_human { human_tags } else { bot_tags }))
        }
        // human blocked, always block, even if it is a bot
        AclResult::Match(BotHuman {
            bot: _,
            human: Some(AclDecision {
                allowed: false,
                tags: dtags,
            }),
        }) => {
            logs.debug("ACL human block detected");
            Some((5, dtags))
        }
        // robot blocked, should be challenged
        AclResult::Match(BotHuman {
            bot: Some(AclDecision {
                allowed: false,
                tags: dtags,
            }),
            human: _,
        }) => {
            if is_human {
                None
            } else {
                match (reqinfo.headers.get("user-agent"), mgh) {
                    (Some(ua), Some(gh)) => {
                        logs.debug("ACL challenge detected: challenged");
                        return (
                            challenge_phase01(&gh, ua, dtags),
                            tags,
                            masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
                        );
                    }
                    (gua, ggh) => {
                        logs.debug(format!(
                            "ACL challenge detected: can't challenge, ua={} gh={}",
                            gua.is_some(),
                            ggh.is_some()
                        ));
                        Some((3, dtags))
                    }
                }
            }
        }
        _ => None,
    };
    logs.debug(format!("ACL checks done {:?}", blockcode));

    // if the acl is active, and we had a block result, immediately block
    if securitypolicy.acl_active {
        if let Some((cde, tgs)) = blockcode {
            return (
                acl_block(true, cde, &tgs),
                tags,
                masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
            );
        }
    }

    // otherwise, run content_filter_check
    let content_filter_result = match HSDB.read() {
        Ok(rd) => content_filter_check(logs, &mut tags, &reqinfo, &securitypolicy.content_filter_profile, rd),
        Err(rr) => {
            logs.error(format!("Could not get lock on HSDB: {}", rr));
            Ok(())
        }
    };
    logs.debug("Content Filter checks done");

    (
        match content_filter_result {
            Ok(()) => {
                // if content filter was ok, but we had an acl decision, return the monitored acl decision for logged purposes
                if let Some((cde, tgs)) = blockcode {
                    acl_block(false, cde, &tgs)
                } else {
                    Decision::Pass
                }
            }
            Err(wb) => {
                let mut action = wb.to_action();
                action.block_mode &= securitypolicy.content_filter_active;
                Decision::Action(action)
            }
        },
        tags,
        masking(masking_seed, reqinfo, &securitypolicy.content_filter_profile),
    )
}

// generic entry point when the request map has already been parsed
pub fn content_filter_check_generic_request_map(
    configpath: &str,
    raw: &RawRequest,
    content_filter_id: &str,
    logs: &mut Logs,
) -> (Decision, RequestInfo, Tags) {
    let mut tags = Tags::default();
    logs.debug("Content Filter inspection starts");
    let waf_profile = match with_config(configpath, logs, |_slogs, cfg| {
        cfg.content_filter_profiles.get(content_filter_id).cloned()
    }) {
        Some(Some(prof)) => prof,
        _ => {
            logs.error("Content Filter profile not found");
            return (Decision::Pass, map_request(logs, &[], &[], raw), tags);
        }
    };

    let reqinfo = map_request(logs, &waf_profile.decoding, &[], raw);

    let waf_result = match HSDB.read() {
        Ok(rd) => content_filter_check(logs, &mut tags, &reqinfo, &waf_profile, rd),
        Err(rr) => {
            logs.error(format!("Could not get lock on HSDB: {}", rr));
            Ok(())
        }
    };
    logs.debug("Content Filter checks done");

    (
        match waf_result {
            Ok(()) => Decision::Pass,
            Err(wb) => Decision::Action(wb.to_action()),
        },
        reqinfo,
        tags,
    )
}
