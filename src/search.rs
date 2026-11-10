//! The search (`GET /v2/search`): the query of the user (words, "phrases", -exclusions) in a mode, on some
//! fields, filtered by kind, extension and modification date; the best match of each file (or every match) with a
//! highlighted snippet.

use std::{collections::HashMap, fmt::Display, ops::Bound, time::Instant};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tantivy::{
    DateTime, TantivyDocument, Term,
    collector::{Count, TopDocs},
    query::{
        AllQuery, BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, QueryClone,
        QueryParser, RangeQuery, RegexQuery, TermQuery, TermSetQuery,
    },
    schema::{Field, IndexRecordOption, Value},
};

use crate::index_tantivy::FileSearchIndex;
use crate::utils::KINDS;

pub const MAX_PER_PAGE: usize = 100;
/// deep pages are read from the first match: bounded
pub const MAX_PAGE: usize = 1000;
pub const DEFAULT_SNIPPET_CHARS: usize = 240;
pub const MAX_SNIPPET_CHARS: usize = 1000;

#[derive(Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// every word, exactly, as the beginning of a word or with a typo; the exact phrase ranks first
    #[default]
    Smart,
    /// every word exactly
    Exact,
    /// every word exactly or as the beginning of a word
    Prefix,
    /// every word exactly or with typos (`fuzziness`: 1 or 2 letters, by default from the length of the word)
    Fuzzy,
    /// the words in this order
    Phrase,
    /// a regular expression matching a word (lower case, without accents)
    Regex,
    /// the tantivy query syntax: `invoice AND (2026 OR 2027) -draft`, `cell_ctx:amount`, `"a b"~2`...
    Query,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SearchRequest {
    pub q: String,
    pub mode: SearchMode,
    /// comma separated: content, file_name, sheet, context. Default: all
    pub fields: Option<String>,
    /// comma separated kinds: pdf, spreadsheet, csv, document, image, text
    pub kinds: Option<String>,
    /// comma separated extensions (pdf, xlsx...)
    pub extensions: Option<String>,
    /// yyyy-mm-dd, the file modified this day or after
    pub modified_from: Option<String>,
    /// yyyy-mm-dd, the file modified this day or before
    pub modified_to: Option<String>,
    /// 0, 1 or 2 letters of difference for the fuzzy and smart modes. Default: from the length of the word
    pub fuzziness: Option<u8>,
    /// zero based
    pub page: usize,
    pub per_page: usize,
    /// true: the best match of each file. false: every match (each row of a spreadsheet, each chunk of a pdf)
    pub group_by_file: bool,
    pub snippet_chars: Option<usize>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            q: String::new(),
            mode: SearchMode::Smart,
            fields: None,
            kinds: None,
            extensions: None,
            modified_from: None,
            modified_to: None,
            fuzziness: None,
            page: 0,
            per_page: 20,
            group_by_file: true,
            snippet_chars: None,
        }
    }
}

#[derive(Serialize, Debug)]
pub struct SearchHit {
    pub score: f32,
    pub source_path: String,
    /// the uploaded file: the file itself, or the archive it was extracted from
    pub origin_path: String,
    pub file_name: String,
    pub origin_file_name: String,
    pub kind: String,
    pub extension: String,
    pub sheet_name: Option<String>,
    /// cells of the row (A2, B2...) or the chunk ("chunk 3")
    pub positions: Vec<String>,
    /// column labels of the row
    pub context: Vec<String>,
    /// the text of the match (cells of the row joined by " | ")
    pub content: String,
    /// part of the content with the matching words in <b></b>, html escaped
    pub snippet_html: String,
    /// yyyy-mm-dd
    pub modified_at: Option<String>,
    /// matches in this file (rows, chunks); 1 when not grouped by file
    pub matches: usize,
}

#[derive(Serialize, Debug)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub page: usize,
    pub per_page: usize,
    pub has_more: bool,
    /// matching rows / chunks, all files
    pub total_matches: usize,
    pub took_ms: u128,
}

#[derive(Debug)]
pub enum SearchError {
    /// the request is wrong (400)
    BadRequest(String),
    Internal(String),
}

impl Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::BadRequest(m) => write!(f, "{m}"),
            SearchError::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl From<tantivy::TantivyError> for SearchError {
    fn from(e: tantivy::TantivyError) -> Self {
        SearchError::Internal(e.to_string())
    }
}

// ---------- the query of the user ----------

/// A part of the query of the user.
#[derive(Debug, PartialEq, Eq)]
pub enum Clause {
    /// a word (or a group like INV-2026-001, matched as a phrase of its words)
    Word { text: String, negated: bool },
    /// "quoted words"
    Phrase { text: String, negated: bool },
}

/// Words, "phrases" and -exclusions. An unclosed quote quotes until the end.
pub fn parse_clauses(q: &str) -> Vec<Clause> {
    let mut clauses = Vec::new();
    let mut chars = q.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut negated = false;
        if c == '-' {
            chars.next();
            match chars.peek() {
                Some(next) if !next.is_whitespace() => negated = true,
                _ => continue,
            }
        }
        if chars.peek() == Some(&'"') {
            chars.next();
            let text: String = chars.by_ref().take_while(|c| *c != '"').collect();
            if !text.trim().is_empty() {
                clauses.push(Clause::Phrase { text, negated });
            }
        } else {
            let mut text = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                text.push(c);
                chars.next();
            }
            clauses.push(Clause::Word { text, negated });
        }
    }
    clauses
}

/// Letters of difference allowed for a word: none under 4 letters, 1 under 8, 2 otherwise.
pub fn auto_fuzziness(word: &str) -> u8 {
    match word.chars().count() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// The searched fields and their weight.
fn search_fields(
    index: &FileSearchIndex,
    fields: Option<&str>,
) -> Result<Vec<(Field, f32)>, SearchError> {
    let all = [
        ("content", index.cell_value_field, 1.0),
        ("file_name", index.file_name_text_field, 2.0),
        ("sheet", index.sheet_name_field, 1.5),
        ("context", index.cell_ctx_field, 1.2),
    ];
    let Some(fields) = fields.map(str::trim).filter(|f| !f.is_empty()) else {
        return Ok(all.iter().map(|(_, f, b)| (*f, *b)).collect());
    };
    fields
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|name| {
            all.iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, f, b)| (*f, *b))
                .ok_or_else(|| {
                    SearchError::BadRequest(format!(
                        "unknown field {name} (content, file_name, sheet, context)"
                    ))
                })
        })
        .collect()
}

fn boxed(query: impl Query + 'static) -> Box<dyn Query> {
    Box::new(query)
}

fn boost(query: Box<dyn Query>, boost: f32) -> Box<dyn Query> {
    if (boost - 1.0).abs() < f32::EPSILON {
        query
    } else {
        Box::new(BoostQuery::new(query, boost))
    }
}

fn should(queries: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    if queries.len() == 1 {
        return queries.into_iter().next().unwrap();
    }
    Box::new(BooleanQuery::new(
        queries.into_iter().map(|q| (Occur::Should, q)).collect(),
    ))
}

/// One word (already analyzed) in a field.
fn word_query(
    field: Field,
    token: &str,
    mode: SearchMode,
    fuzziness: Option<u8>,
) -> Box<dyn Query> {
    let term = Term::from_field_text(field, token);
    let exact = || boxed(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
    let prefix = || boxed(FuzzyTermQuery::new_prefix(term.clone(), 0, true));
    let distance = fuzziness.unwrap_or_else(|| auto_fuzziness(token)).min(2);
    let fuzzy = || boxed(FuzzyTermQuery::new(term.clone(), distance, true));
    match mode {
        SearchMode::Exact | SearchMode::Phrase | SearchMode::Regex | SearchMode::Query => exact(),
        SearchMode::Prefix => should(vec![boost(exact(), 3.0), prefix()]),
        SearchMode::Fuzzy if distance == 0 => exact(),
        SearchMode::Fuzzy => should(vec![boost(exact(), 3.0), fuzzy()]),
        SearchMode::Smart => {
            let mut alternatives = vec![boost(exact(), 4.0)];
            if token.chars().count() >= 3 {
                alternatives.push(boost(prefix(), 2.0));
            }
            if distance > 0 {
                alternatives.push(fuzzy());
            }
            should(alternatives)
        }
    }
}

/// Words in this order in a field (one word: the word exactly).
fn phrase_query(field: Field, tokens: &[String]) -> Box<dyn Query> {
    if tokens.len() == 1 {
        return boxed(TermQuery::new(
            Term::from_field_text(field, &tokens[0]),
            IndexRecordOption::WithFreqs,
        ));
    }
    boxed(PhraseQuery::new(
        tokens
            .iter()
            .map(|t| Term::from_field_text(field, t))
            .collect(),
    ))
}

/// A clause in any of the fields.
fn clause_query(
    index: &FileSearchIndex,
    fields: &[(Field, f32)],
    clause: &Clause,
    mode: SearchMode,
    fuzziness: Option<u8>,
) -> Option<Box<dyn Query>> {
    let (text, phrase) = match clause {
        Clause::Word { text, .. } => (text, false),
        Clause::Phrase { text, .. } => (text, true),
    };
    let tokens = index.analyze(text);
    if tokens.is_empty() {
        return None;
    }
    Some(should(
        fields
            .iter()
            .map(|(field, field_boost)| {
                let query = if phrase || tokens.len() > 1 {
                    phrase_query(*field, &tokens)
                } else {
                    word_query(*field, &tokens[0], mode, fuzziness)
                };
                boost(query, *field_boost)
            })
            .collect(),
    ))
}

/// The query of the text of the request (without the filters). `None`: nothing to search (empty, or only
/// punctuation).
fn text_query(
    index: &FileSearchIndex,
    request: &SearchRequest,
    fields: &[(Field, f32)],
) -> Result<Option<Box<dyn Query>>, SearchError> {
    let q = request.q.trim();
    if q.is_empty() {
        return Ok(None);
    }
    match request.mode {
        SearchMode::Regex => {
            // the indexed words are lower case without accents
            let pattern = q.to_lowercase();
            let queries = fields
                .iter()
                .map(|(field, field_boost)| {
                    RegexQuery::from_pattern(&pattern, *field)
                        .map(|query| boost(boxed(query), *field_boost))
                        .map_err(|e| SearchError::BadRequest(format!("invalid regex: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(should(queries)))
        }
        SearchMode::Query => {
            let mut parser =
                QueryParser::for_index(&index.index, fields.iter().map(|(f, _)| *f).collect());
            parser.set_conjunction_by_default();
            for (field, field_boost) in fields {
                parser.set_field_boost(*field, *field_boost);
            }
            let (query, errors) = parser.parse_query_lenient(q);
            if !errors.is_empty() {
                tracing::debug!("query {q:?} parsed leniently: {errors:?}");
            }
            Ok(Some(query))
        }
        SearchMode::Phrase => Ok(clause_query(
            index,
            fields,
            &Clause::Phrase {
                text: q.to_string(),
                negated: false,
            },
            request.mode,
            request.fuzziness,
        )),
        SearchMode::Smart | SearchMode::Exact | SearchMode::Prefix | SearchMode::Fuzzy => {
            let clauses = parse_clauses(q);
            let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut positive_words = Vec::new();
            for clause in &clauses {
                let negated = match clause {
                    Clause::Word { negated, .. } | Clause::Phrase { negated, .. } => *negated,
                };
                // an excluded word is excluded exactly, never by prefix or typo
                let mode = if negated {
                    SearchMode::Exact
                } else {
                    request.mode
                };
                let Some(query) = clause_query(index, fields, clause, mode, request.fuzziness)
                else {
                    continue;
                };
                if negated {
                    subqueries.push((Occur::MustNot, query));
                } else {
                    if let Clause::Word { text, .. } = clause {
                        positive_words.push(String::as_str(text));
                    }
                    subqueries.push((Occur::Must, query));
                }
            }
            if subqueries.is_empty() {
                return Ok(None);
            }
            if subqueries.iter().all(|(occur, _)| *occur == Occur::MustNot) {
                // only exclusions: everything but them
                subqueries.push((Occur::Must, boxed(AllQuery)));
            }
            // the words next to each other rank first
            if request.mode == SearchMode::Smart && positive_words.len() > 1 {
                let tokens = index.analyze(&positive_words.join(" "));
                subqueries.push((
                    Occur::Should,
                    boost(phrase_query(index.cell_value_field, &tokens), 2.0),
                ));
            }
            Ok(Some(boxed(BooleanQuery::new(subqueries))))
        }
    }
}

fn list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

fn date(value: Option<&str>, name: &str) -> Result<Option<NaiveDate>, SearchError> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| {
                SearchError::BadRequest(format!("{name}: yyyy-mm-dd expected, got {v}"))
            })
        })
        .transpose()
}

fn start_of(day: NaiveDate) -> DateTime {
    DateTime::from_timestamp_secs(day.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

/// The filters of the request (kinds, extensions, modification dates).
fn filters(
    index: &FileSearchIndex,
    request: &SearchRequest,
) -> Result<Vec<Box<dyn Query>>, SearchError> {
    let mut filters: Vec<Box<dyn Query>> = Vec::new();
    let kinds = list(request.kinds.as_deref());
    if let Some(unknown) = kinds.iter().find(|k| !KINDS.contains(&String::as_str(k))) {
        return Err(SearchError::BadRequest(format!(
            "unknown kind {unknown} ({})",
            KINDS.join(", ")
        )));
    }
    if !kinds.is_empty() {
        filters.push(boxed(TermSetQuery::new(
            kinds
                .iter()
                .map(|k| Term::from_field_text(index.kind_field, k)),
        )));
    }
    let extensions = list(request.extensions.as_deref());
    if !extensions.is_empty() {
        filters.push(boxed(TermSetQuery::new(
            extensions
                .iter()
                .map(|e| Term::from_field_text(index.extension_field, e)),
        )));
    }
    let from = date(request.modified_from.as_deref(), "modified_from")?;
    let to = date(request.modified_to.as_deref(), "modified_to")?;
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err(SearchError::BadRequest(
            "modified_from is after modified_to".to_string(),
        ));
    }
    if from.is_some() || to.is_some() {
        let field = index.modified_at_field;
        let lower = from
            .map(|d| Bound::Included(Term::from_field_date_for_search(field, start_of(d))))
            .unwrap_or(Bound::Unbounded);
        // the whole last day: before the start of the next one
        let upper = to
            .and_then(|d| d.succ_opt())
            .map(|d| Bound::Excluded(Term::from_field_date_for_search(field, start_of(d))))
            .unwrap_or(Bound::Unbounded);
        filters.push(boxed(RangeQuery::new(lower, upper)));
    }
    Ok(filters)
}

/// The query of a request: the text and the filters. An empty text with filters: every document filtered.
pub fn build_query(
    index: &FileSearchIndex,
    request: &SearchRequest,
) -> Result<Box<dyn Query>, SearchError> {
    let fields = search_fields(index, request.fields.as_deref())?;
    let text = text_query(index, request, &fields)?;
    let filters = filters(index, request)?;
    if text.is_none() && filters.is_empty() {
        return Err(SearchError::BadRequest(
            "nothing to search: a text or a filter is required".to_string(),
        ));
    }
    if filters.is_empty() {
        return Ok(text.unwrap());
    }
    let mut subqueries = vec![(Occur::Must, text.unwrap_or_else(|| boxed(AllQuery)))];
    subqueries.extend(filters.into_iter().map(|f| (Occur::Must, f)));
    Ok(boxed(BooleanQuery::new(subqueries)))
}

// ---------- highlighting ----------

/// A word of the query, as it is matched.
#[derive(Debug, Clone)]
pub struct HighlightTerm {
    pub token: String,
    pub prefix: bool,
    pub distance: u8,
}

/// The words of the query to highlight, matched like the query does.
pub fn highlight_terms(index: &FileSearchIndex, request: &SearchRequest) -> Vec<HighlightTerm> {
    let terms = |text: &str, mode: SearchMode| -> Vec<HighlightTerm> {
        let tokens = index.analyze(text);
        let single = tokens.len() == 1;
        tokens
            .into_iter()
            .map(|token| {
                let distance = if !single {
                    0
                } else {
                    match mode {
                        SearchMode::Smart | SearchMode::Fuzzy => request
                            .fuzziness
                            .unwrap_or_else(|| auto_fuzziness(&token))
                            .min(2),
                        _ => 0,
                    }
                };
                let prefix = single
                    && (mode == SearchMode::Prefix
                        || (mode == SearchMode::Smart && token.chars().count() >= 3));
                HighlightTerm {
                    token,
                    prefix,
                    distance,
                }
            })
            .collect()
    };
    match request.mode {
        // the words of the expression (operators and field names highlight nothing)
        SearchMode::Query => terms(&request.q, SearchMode::Exact),
        SearchMode::Regex => Vec::new(),
        SearchMode::Phrase => terms(&request.q, SearchMode::Exact),
        mode => parse_clauses(&request.q)
            .into_iter()
            .flat_map(|clause| match clause {
                Clause::Word { negated: true, .. } | Clause::Phrase { negated: true, .. } => {
                    Vec::new()
                }
                Clause::Word { text, .. } => terms(&text, mode),
                Clause::Phrase { text, .. } => terms(&text, SearchMode::Exact),
            })
            .collect(),
    }
}

/// Edit distance (with transpositions) of two short words, stopping above `max`.
fn within_distance(a: &str, b: &str, max: u8) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let max = max as usize;
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut previous2: Vec<usize> = vec![0; b.len() + 1];
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut current = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                current[j] = current[j].min(previous2[j - 2] + 1);
            }
        }
        previous2 = std::mem::replace(&mut previous, current);
    }
    previous[b.len()] <= max
}

fn matches_term(word: &str, term: &HighlightTerm) -> bool {
    word == term.token
        || (term.prefix && word.starts_with(&term.token))
        || (term.distance > 0 && within_distance(word, &term.token, term.distance))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The part of the text (at most `max_chars`) with the most matching words, those in <b></b>; html escaped.
pub fn highlight(
    index: &FileSearchIndex,
    text: &str,
    terms: &[HighlightTerm],
    max_chars: usize,
) -> String {
    // (start, end) byte offsets of the words of the text matching a term
    let mut matched: Vec<(usize, usize)> = Vec::new();
    if !terms.is_empty()
        && let Some(mut analyzer) = index
            .index
            .tokenizers()
            .get(crate::index_tantivy::FOLDED_TOKENIZER)
    {
        let mut stream = analyzer.token_stream(text);
        use tantivy::tokenizer::TokenStream;
        while stream.advance() {
            let token = stream.token();
            if terms.iter().any(|t| matches_term(&token.text, t)) {
                matched.push((token.offset_from, token.offset_to));
            }
        }
    }
    let boundary = |mut i: usize| {
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
        i.min(text.len())
    };
    // the window starting a little before the match with the most matches after it
    let (start, end) = if matched.is_empty() {
        (0, boundary(max_chars))
    } else {
        let lead = max_chars / 5;
        let best = matched
            .iter()
            .map(|(from, _)| {
                let start = from.saturating_sub(lead);
                let count = matched
                    .iter()
                    .filter(|(f, t)| *f >= start && *t <= start + max_chars)
                    .count();
                (count, std::cmp::Reverse(start))
            })
            .max()
            .map(|(_, std::cmp::Reverse(start))| start)
            .unwrap_or(0);
        let mut start = boundary(best);
        // start at the beginning of a word
        if start > 0
            && let Some(space) = text[..start].rfind(char::is_whitespace)
            && start - space < 20
        {
            start = space + 1;
        }
        (start, boundary(start + max_chars))
    };
    let mut html = String::new();
    if start > 0 {
        html.push('…');
    }
    let mut position = start;
    for (from, to) in matched.iter().filter(|(f, t)| *f >= start && *t <= end) {
        html.push_str(&escape(&text[position..*from]));
        html.push_str("<b>");
        html.push_str(&escape(&text[*from..*to]));
        html.push_str("</b>");
        position = *to;
    }
    html.push_str(&escape(&text[position..end]));
    if end < text.len() {
        html.push('…');
    }
    html
}

// ---------- the search ----------

fn texts(doc: &TantivyDocument, field: Field) -> Vec<String> {
    doc.get_all(field)
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn file_name_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn search(
    index: &FileSearchIndex,
    request: &SearchRequest,
) -> Result<SearchResponse, SearchError> {
    let started = Instant::now();
    if request.per_page == 0 || request.per_page > MAX_PER_PAGE {
        return Err(SearchError::BadRequest(format!(
            "per_page: between 1 and {MAX_PER_PAGE}"
        )));
    }
    if request.page > MAX_PAGE {
        return Err(SearchError::BadRequest(format!("page: at most {MAX_PAGE}")));
    }
    let query = build_query(index, request)?;
    let searcher = index.index_reader.searcher();
    let total_matches = searcher.search(&*query, &Count)?;

    // one more than the page: is there a next page
    let docs: Vec<(f32, TantivyDocument)> = if request.group_by_file {
        let skip = request.page * request.per_page;
        // page * per_page files, then per_page + 1: the page and one more
        let mut docs = index
            .best_per_file(0, skip + request.per_page + 1, &*query)
            .map_err(|e| SearchError::Internal(e.to_string()))?;
        docs.drain(..skip.min(docs.len()));
        docs
    } else {
        searcher
            .search(
                &*query,
                &TopDocs::with_limit(request.per_page + 1)
                    .and_offset(request.page * request.per_page)
                    .order_by_score(),
            )?
            .into_iter()
            .map(|(score, address)| searcher.doc(address).map(|doc| (score, doc)))
            .collect::<Result<_, _>>()?
    };
    let has_more = docs.len() > request.per_page;

    let terms = highlight_terms(index, request);
    let snippet_chars = request
        .snippet_chars
        .unwrap_or(DEFAULT_SNIPPET_CHARS)
        .clamp(40, MAX_SNIPPET_CHARS);
    let mut matches_of_source: HashMap<String, usize> = HashMap::new();
    let mut hits = Vec::new();
    for (score, doc) in docs.into_iter().take(request.per_page) {
        let source_path = text(&doc, index.source_path_field);
        let matches = if request.group_by_file {
            match matches_of_source.get(&source_path) {
                Some(count) => *count,
                None => {
                    let in_file = BooleanQuery::new(vec![
                        (Occur::Must, query.box_clone()),
                        (
                            Occur::Must,
                            boxed(TermQuery::new(
                                Term::from_field_text(index.source_path_field, &source_path),
                                IndexRecordOption::Basic,
                            )),
                        ),
                    ]);
                    let count = searcher.search(&in_file, &Count)?;
                    matches_of_source.insert(source_path.clone(), count);
                    count
                }
            }
        } else {
            1
        };
        let content = texts(&doc, index.cell_value_field).join(" | ");
        let origin_path = Some(text(&doc, index.origin_path_field))
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| source_path.clone());
        hits.push(SearchHit {
            score,
            snippet_html: highlight(index, &content, &terms, snippet_chars),
            content,
            file_name: text(&doc, index.file_name_field),
            origin_file_name: file_name_of(&origin_path),
            origin_path,
            source_path,
            kind: text(&doc, index.kind_field),
            extension: text(&doc, index.extension_field),
            sheet_name: Some(text(&doc, index.sheet_name_field)).filter(|s| !s.is_empty()),
            positions: texts(&doc, index.cell_position_field),
            context: texts(&doc, index.cell_ctx_field),
            modified_at: doc
                .get_first(index.modified_at_field)
                .and_then(|v| v.as_datetime())
                .and_then(|d| chrono::DateTime::from_timestamp(d.into_timestamp_secs(), 0))
                .map(|d| d.format("%Y-%m-%d").to_string()),
            matches,
        });
    }
    Ok(SearchResponse {
        hits,
        page: request.page,
        per_page: request.per_page,
        has_more,
        total_matches,
        took_ms: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_tantivy::test_support::{add_docs, temp_index};

    fn request(q: &str, mode: SearchMode) -> SearchRequest {
        SearchRequest {
            q: q.to_string(),
            mode,
            ..Default::default()
        }
    }

    fn files(response: &SearchResponse) -> Vec<&str> {
        response
            .hits
            .iter()
            .map(|h| String::as_str(&h.file_name))
            .collect()
    }

    #[test]
    fn clauses_of_a_query() {
        assert_eq!(
            parse_clauses(r#"facture "frais de port" -brouillon - INV-2026"#),
            vec![
                Clause::Word {
                    text: "facture".into(),
                    negated: false
                },
                Clause::Phrase {
                    text: "frais de port".into(),
                    negated: false
                },
                Clause::Word {
                    text: "brouillon".into(),
                    negated: true
                },
                Clause::Word {
                    text: "INV-2026".into(),
                    negated: false
                },
            ]
        );
        assert_eq!(
            parse_clauses(r#"-"not this" "unclosed quote"#),
            vec![
                Clause::Phrase {
                    text: "not this".into(),
                    negated: true
                },
                Clause::Phrase {
                    text: "unclosed quote".into(),
                    negated: false
                },
            ]
        );
        assert!(parse_clauses("   ").is_empty());
    }

    #[test]
    fn fuzziness_from_length() {
        assert_eq!(auto_fuzziness("tva"), 0);
        assert_eq!(auto_fuzziness("facture"), 1);
        assert_eq!(auto_fuzziness("consultance"), 2);
    }

    #[test]
    fn distances() {
        assert!(within_distance("invoice", "invioce", 1));
        assert!(within_distance("facture", "factur", 1));
        assert!(!within_distance("facture", "fracture", 0));
        assert!(within_distance("facture", "fracture", 1));
        assert!(!within_distance("abc", "xyz", 2));
    }

    #[tokio::test]
    async fn smart_search_matches_accents_typos_and_prefixes() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "Facture de consultance pour octobre"),
                ("b.xlsx", "spreadsheet", "Montant: 1200 | Client: ACME"),
                ("c.txt", "text", "Rapport annuel"),
            ],
        )
        .await;
        let search = |q: &str| search(&index, &request(q, SearchMode::Smart)).unwrap();
        assert_eq!(files(&search("facturé")), vec!["a.pdf"]);
        assert_eq!(files(&search("factur")), vec!["a.pdf"]);
        assert_eq!(files(&search("consultnce octobre")), vec!["a.pdf"]);
        assert_eq!(files(&search("acme montant")), vec!["b.xlsx"]);
        assert!(files(&search("facture -octobre")).is_empty());
        assert_eq!(files(&search("-facture rapport")), vec!["c.txt"]);
        // file names are searched too
        assert_eq!(files(&search("b.xlsx")), vec!["b.xlsx"]);
    }

    #[tokio::test]
    async fn exact_and_phrase_modes() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "frais de port offerts"),
                ("b.pdf", "pdf", "port de frais"),
                ("c.pdf", "pdf", "les frais port inclus"),
            ],
        )
        .await;
        let exact = search(&index, &request("frai", SearchMode::Exact)).unwrap();
        assert!(exact.hits.is_empty());
        let phrase = search(&index, &request("frais de port", SearchMode::Phrase)).unwrap();
        assert_eq!(files(&phrase), vec!["a.pdf"]);
        let quoted = search(&index, &request("\"frais de port\"", SearchMode::Smart)).unwrap();
        assert_eq!(files(&quoted), vec!["a.pdf"]);
        let smart = search(&index, &request("frais port", SearchMode::Smart)).unwrap();
        assert_eq!(files(&smart).len(), 3);
        assert_eq!(
            files(&smart)[0],
            "c.pdf",
            "the words next to each other first"
        );
    }

    #[tokio::test]
    async fn regex_and_query_modes() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "INV-2026-001 consulting"),
                ("b.pdf", "pdf", "INV-2025-104 hosting"),
            ],
        )
        .await;
        let regex = search(&index, &request("202[56]", SearchMode::Regex)).unwrap();
        assert_eq!(regex.hits.len(), 2);
        let query = search(&index, &request("inv AND hosting", SearchMode::Query)).unwrap();
        assert_eq!(files(&query), vec!["b.pdf"]);
        let invalid = search(&index, &request("(", SearchMode::Regex));
        assert!(matches!(invalid, Err(SearchError::BadRequest(_))));
    }

    #[tokio::test]
    async fn filters_by_kind_extension_and_date() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "invoice"),
                ("b.xlsx", "spreadsheet", "invoice"),
                ("c.csv", "csv", "invoice"),
            ],
        )
        .await;
        let mut r = request("invoice", SearchMode::Smart);
        r.kinds = Some("spreadsheet, csv".into());
        let response = search(&index, &r).unwrap();
        let mut found = files(&response);
        found.sort();
        assert_eq!(found, vec!["b.xlsx", "c.csv"]);

        let mut r = request("", SearchMode::Smart);
        r.extensions = Some(".PDF".into());
        assert_eq!(files(&search(&index, &r).unwrap()), vec!["a.pdf"]);

        // documents of add_docs are modified at 1_790_000_000 (2026-09-21)
        let mut r = request("invoice", SearchMode::Smart);
        r.modified_from = Some("2026-09-21".into());
        r.modified_to = Some("2026-09-21".into());
        assert_eq!(search(&index, &r).unwrap().hits.len(), 3);
        r.modified_from = Some("2026-09-22".into());
        r.modified_to = None;
        assert!(search(&index, &r).unwrap().hits.is_empty());

        let mut r = request("invoice", SearchMode::Smart);
        r.kinds = Some("video".into());
        assert!(matches!(
            search(&index, &r),
            Err(SearchError::BadRequest(_))
        ));
        let r = request("  ", SearchMode::Smart);
        assert!(matches!(
            search(&index, &r),
            Err(SearchError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn pages_snippets_and_metadata() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "invoice one"),
                ("b.pdf", "pdf", "invoice two"),
                ("c.pdf", "pdf", "invoice three"),
            ],
        )
        .await;
        let mut r = request("invoce", SearchMode::Smart);
        r.per_page = 2;
        let first = search(&index, &r).unwrap();
        assert_eq!(first.hits.len(), 2);
        assert!(first.has_more);
        assert_eq!(first.total_matches, 3);
        r.page = 1;
        let second = search(&index, &r).unwrap();
        assert_eq!(second.hits.len(), 1);
        assert!(!second.has_more);

        let hit = &first.hits[0];
        assert!(
            hit.snippet_html.contains("<b>invoice</b>"),
            "{}",
            hit.snippet_html
        );
        assert_eq!(hit.kind, "pdf");
        assert_eq!(hit.extension, "pdf");
        assert_eq!(hit.modified_at.as_deref(), Some("2026-09-21"));
        assert_eq!(hit.matches, 1);
        assert_eq!(hit.origin_file_name, hit.file_name);
    }

    #[test]
    fn snippet_window_and_escaping() {
        let index = temp_index();
        let text = format!(
            "{} <tag> facture & co {}",
            "x ".repeat(200),
            "y ".repeat(200)
        );
        let html = highlight(
            &index,
            &text,
            &[HighlightTerm {
                token: "facture".into(),
                prefix: false,
                distance: 0,
            }],
            80,
        );
        assert!(html.starts_with('…') && html.ends_with('…'));
        assert!(
            html.contains("&lt;tag&gt; <b>facture</b> &amp; co"),
            "{html}"
        );
        assert!(html.chars().count() < 120);
        // nothing matched: the beginning
        let html = highlight(&index, "short text", &[], 80);
        assert_eq!(html, "short text");
    }
}
