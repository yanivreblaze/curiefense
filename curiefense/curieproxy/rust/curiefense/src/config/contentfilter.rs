use crate::config::raw::{
    ContentFilterGroup, ContentFilterRule, ContentType, RawContentFilterEntryMatch, RawContentFilterProfile,
    RawContentFilterProperties,
};
use crate::config::utils::Matching;
use crate::logs::Logs;

use hyperscan::prelude::{pattern, Builder, CompileFlags, Pattern, Patterns, VectoredDatabase};
use hyperscan::Vectored;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;

#[derive(Debug, Clone)]
pub struct Section<A> {
    pub headers: A,
    pub cookies: A,
    pub args: A,
    pub path: A,
}

#[derive(Debug, Clone)]
pub struct ContentFilterProfile {
    pub id: String,
    pub name: String,
    pub active: HashSet<String>,
    pub ignore: HashSet<String>,
    pub report: HashSet<String>,
    pub ignore_alphanum: bool,
    pub sections: Section<ContentFilterSection>,
    pub decoding: Vec<Transformation>,
    pub masking_seed: Vec<u8>,
    pub content_type: Vec<ContentType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transformation {
    Base64Decode,
    HtmlEntitiesDecode,
    UnicodeDecode,
    UrlDecode,
}

impl ContentFilterProfile {
    pub fn default_from_seed(seed: &str) -> Self {
        ContentFilterProfile {
            id: "__default__".to_string(),
            name: "default contentfilter".to_string(),
            ignore_alphanum: true,
            sections: Section {
                headers: ContentFilterSection {
                    max_count: 42,
                    max_length: 1024,
                    names: HashMap::new(),
                    regex: Vec::new(),
                },
                args: ContentFilterSection {
                    max_count: 512,
                    max_length: 1024,
                    names: HashMap::new(),
                    regex: Vec::new(),
                },
                cookies: ContentFilterSection {
                    max_count: 42,
                    max_length: 1024,
                    names: HashMap::new(),
                    regex: Vec::new(),
                },
                path: ContentFilterSection {
                    max_count: 42,
                    max_length: 1024,
                    names: HashMap::new(),
                    regex: Vec::new(),
                },
            },
            decoding: vec![Transformation::Base64Decode, Transformation::UrlDecode],
            masking_seed: seed.as_bytes().to_vec(),
            active: HashSet::default(),
            ignore: HashSet::default(),
            report: HashSet::default(),
            content_type: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContentFilterSection {
    pub max_count: usize,
    pub max_length: usize,
    pub names: HashMap<String, ContentFilterEntryMatch>,
    pub regex: Vec<(Regex, ContentFilterEntryMatch)>,
}

#[derive(Debug, Clone)]
pub struct ContentFilterEntryMatch {
    pub reg: Option<Matching<String>>,
    pub restrict: bool,
    pub mask: bool,
    pub exclusions: HashSet<String>,
}

#[derive(Debug, Clone, Eq, Serialize, PartialEq, Copy)]
#[serde(rename_all = "snake_case")]
pub enum SectionIdx {
    Headers,
    Cookies,
    Args,
    Path,
}

impl<A> Section<A> {
    pub fn get(&self, idx: SectionIdx) -> &A {
        match idx {
            SectionIdx::Headers => &self.headers,
            SectionIdx::Cookies => &self.cookies,
            SectionIdx::Args => &self.args,
            SectionIdx::Path => &self.path,
        }
    }

    pub fn at(&mut self, idx: SectionIdx) -> &mut A {
        match idx {
            SectionIdx::Headers => &mut self.headers,
            SectionIdx::Cookies => &mut self.cookies,
            SectionIdx::Args => &mut self.args,
            SectionIdx::Path => &mut self.path,
        }
    }
}

impl<A> Default for Section<A>
where
    A: Default,
{
    fn default() -> Self {
        Section {
            headers: Default::default(),
            cookies: Default::default(),
            args: Default::default(),
            path: Default::default(),
        }
    }
}

pub struct ContentFilterRules {
    pub db: VectoredDatabase,
    pub ids: Vec<ContentFilterRule>,
}

impl ContentFilterRules {
    pub fn empty() -> Self {
        let pattern: Pattern = pattern! { "^TEST$" };
        ContentFilterRules {
            db: pattern.build().unwrap(),
            ids: Vec::new(),
        }
    }
}

fn mk_entry_match(em: RawContentFilterEntryMatch) -> anyhow::Result<(String, ContentFilterEntryMatch)> {
    let reg = match em.reg {
        None => None,
        Some(s) => {
            if s.is_empty() {
                None
            } else {
                Some(Matching::from_str(&s, s.clone())?)
            }
        }
    };

    Ok((
        em.key,
        ContentFilterEntryMatch {
            restrict: em.restrict,
            mask: em.mask.unwrap_or(false),
            exclusions: em.exclusions.into_iter().collect::<HashSet<_>>(),
            reg,
        },
    ))
}

fn mk_section(props: RawContentFilterProperties) -> anyhow::Result<ContentFilterSection> {
    let mnames: anyhow::Result<HashMap<String, ContentFilterEntryMatch>> =
        props.names.into_iter().map(mk_entry_match).collect();
    let mregex: anyhow::Result<Vec<(Regex, ContentFilterEntryMatch)>> = props
        .regex
        .into_iter()
        .map(|e| {
            let (s, v) = mk_entry_match(e)?;
            let re = Regex::new(&s)?;
            Ok((re, v))
        })
        .collect();
    Ok(ContentFilterSection {
        max_count: props.max_count.0,
        max_length: props.max_length.0,
        names: mnames?,
        regex: mregex?,
    })
}

fn convert_entry(entry: RawContentFilterProfile) -> anyhow::Result<(String, ContentFilterProfile)> {
    let mut decoding = Vec::new();
    // default order
    if entry.decoding.base64 {
        decoding.push(Transformation::Base64Decode)
    }
    if entry.decoding.dual {
        decoding.push(Transformation::UrlDecode)
    }
    if entry.decoding.html {
        decoding.push(Transformation::HtmlEntitiesDecode)
    }
    if entry.decoding.unicode {
        decoding.push(Transformation::UnicodeDecode)
    }
    Ok((
        entry.id.clone(),
        ContentFilterProfile {
            id: entry.id,
            name: entry.name,
            ignore_alphanum: entry.ignore_alphanum,
            sections: Section {
                headers: mk_section(entry.headers)?,
                cookies: mk_section(entry.cookies)?,
                args: mk_section(entry.args)?,
                path: mk_section(entry.path)?,
            },
            decoding,
            masking_seed: entry.masking_seed.as_bytes().to_vec(),
            active: entry.active.into_iter().collect(),
            ignore: entry.ignore.into_iter().collect(),
            report: entry.report.into_iter().collect(),
            content_type: entry.content_type,
        },
    ))
}

impl ContentFilterProfile {
    pub fn resolve(logs: &mut Logs, raw: Vec<RawContentFilterProfile>) -> HashMap<String, ContentFilterProfile> {
        let mut out = HashMap::new();
        for rp in raw {
            let id = rp.id.clone();
            match convert_entry(rp) {
                Ok((k, v)) => {
                    out.insert(k, v);
                }
                Err(rr) => logs.error(format!("content filter id {}: {:?}", id, rr)),
            }
        }
        out
    }
}

fn convert_rule(entry: &ContentFilterRule) -> anyhow::Result<Pattern> {
    Pattern::with_flags(
        &entry.operand,
        CompileFlags::MULTILINE | CompileFlags::DOTALL | CompileFlags::CASELESS,
    )
}

pub fn resolve_rules(
    raws: Vec<ContentFilterRule>,
    groups: Vec<ContentFilterGroup>,
) -> anyhow::Result<ContentFilterRules> {
    let patterns: anyhow::Result<Vec<Pattern>> = raws.iter().map(convert_rule).collect();
    let ptrns: Patterns = Patterns::from_iter(patterns?);

    let mut groupmap: HashMap<String, HashSet<String>> = HashMap::new();
    for group in groups {
        for sig in group.signatures {
            let entry = groupmap.entry(sig).or_default();
            entry.extend(group.tags.iter().cloned());
        }
    }

    // extend the rule tags with the group tags
    let ids = raws
        .into_iter()
        .map(|mut r| {
            if let Some(tgs) = groupmap.get(&r.id) {
                r.tags.extend(tgs.iter().cloned())
            }
            r
        })
        .collect();

    Ok(ContentFilterRules {
        db: ptrns.build::<Vectored>()?,
        ids,
    })
}
