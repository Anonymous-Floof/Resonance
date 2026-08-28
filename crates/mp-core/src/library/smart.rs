//! Smart playlist rules: a saved question rather than a saved list.
//!
//! A rule set describes tracks by their properties — "genre contains shoegaze,
//! added in the last 90 days, least recently played, 50 of them" — and is
//! re-evaluated every time it is opened. That is the whole point: the list is
//! current by construction rather than by the user remembering to update it.
//!
//! # Why this compiles to SQL
//!
//! The obvious implementation loads every track and filters in Rust. It is
//! simpler, and it is wrong at the size this has to work at: a 30k-track
//! library would mean building 30k `Track` structs, each with three resolved
//! strings, to throw almost all of them away. Compiling the rules into a
//! `WHERE` clause lets SQLite use the indexes that already exist and hand back
//! only the rows that matched.
//!
//! # Injection
//!
//! Nothing the user types reaches the SQL text. Column names come from the
//! [`Field`] enum, operators from [`Op`], and every literal travels as a bound
//! parameter — including the `LIKE` patterns, which are assembled with the
//! wildcards escaped so a title containing `%` searches for a percent sign
//! rather than matching everything.

use serde::{Deserialize, Serialize};

use super::model::Order;

/// A track property a rule can test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Title,
    Artist,
    Album,
    Genre,
    Folder,
    Year,
    /// In seconds, as the user thinks of it.
    Duration,
    PlayCount,
    LastPlayed,
    DateAdded,
    Rating,
    /// Whether the metadata came from real tags rather than the filename.
    Tagged,
}

/// What kind of value a field holds, which decides the operators it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    /// A moment in time, compared in days rather than dates.
    Date,
    Flag,
}

impl Field {
    pub const ALL: [Self; 12] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Genre,
        Self::Folder,
        Self::Year,
        Self::Duration,
        Self::PlayCount,
        Self::LastPlayed,
        Self::DateAdded,
        Self::Rating,
        Self::Tagged,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Genre => "Genre",
            Self::Folder => "Folder",
            Self::Year => "Year",
            Self::Duration => "Duration",
            Self::PlayCount => "Play count",
            Self::LastPlayed => "Last played",
            Self::DateAdded => "Date added",
            Self::Rating => "Rating",
            Self::Tagged => "Has tags",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::Title | Self::Artist | Self::Album | Self::Genre | Self::Folder => Kind::Text,
            Self::Year | Self::Duration | Self::PlayCount | Self::Rating => Kind::Number,
            Self::LastPlayed | Self::DateAdded => Kind::Date,
            Self::Tagged => Kind::Flag,
        }
    }

    /// The SQL expression this field compares against.
    ///
    /// Returns `None` for [`Field::Genre`], which is many-to-many and needs a
    /// subquery rather than a column.
    fn column(self) -> Option<&'static str> {
        match self {
            Self::Title => Some("t.title"),
            Self::Artist => Some("ar.name"),
            Self::Album => Some("al.title"),
            Self::Folder => Some("t.folder"),
            Self::Year => Some("t.year"),
            Self::Duration => Some("t.duration_ms"),
            Self::PlayCount => Some("t.play_count"),
            Self::LastPlayed => Some("t.last_played_at"),
            Self::DateAdded => Some("t.added_at"),
            Self::Rating => Some("t.rating"),
            Self::Tagged => Some("t.tagged"),
            Self::Genre => None,
        }
    }
}

/// How a rule compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Is,
    IsNot,
    Contains,
    DoesNotContain,
    StartsWith,
    EndsWith,
    GreaterThan,
    LessThan,
    /// Within the last N days.
    InTheLast,
    /// Not within the last N days — includes "never".
    NotInTheLast,
    IsSet,
    IsNotSet,
}

impl Op {
    pub fn label(self) -> &'static str {
        match self {
            Self::Is => "is",
            Self::IsNot => "is not",
            Self::Contains => "contains",
            Self::DoesNotContain => "does not contain",
            Self::StartsWith => "starts with",
            Self::EndsWith => "ends with",
            Self::GreaterThan => "is more than",
            Self::LessThan => "is less than",
            Self::InTheLast => "in the last",
            Self::NotInTheLast => "not in the last",
            Self::IsSet => "is set",
            Self::IsNotSet => "is not set",
        }
    }

    /// Whether this operator reads a value at all.
    pub fn takes_a_value(self) -> bool {
        !matches!(self, Self::IsSet | Self::IsNotSet)
    }

    /// The operators that make sense for a field of this kind.
    ///
    /// Offering "contains" for a play count, or "is more than" for a title,
    /// produces rules that are either meaningless or quietly always false.
    pub fn for_kind(kind: Kind) -> &'static [Self] {
        match kind {
            Kind::Text => &[
                Self::Is,
                Self::IsNot,
                Self::Contains,
                Self::DoesNotContain,
                Self::StartsWith,
                Self::EndsWith,
                Self::IsSet,
                Self::IsNotSet,
            ],
            Kind::Number => &[
                Self::Is,
                Self::IsNot,
                Self::GreaterThan,
                Self::LessThan,
                Self::IsSet,
                Self::IsNotSet,
            ],
            Kind::Date => &[
                Self::InTheLast,
                Self::NotInTheLast,
                Self::IsSet,
                Self::IsNotSet,
            ],
            Kind::Flag => &[Self::Is],
        }
    }
}

/// One test.
///
/// `value` is kept as text whatever the field, because that is what a rule
/// builder produces and what survives a round trip through JSON unambiguously.
/// It is parsed according to the field's [`Kind`] when the rule is compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub field: Field,
    pub op: Op,
    #[serde(default)]
    pub value: String,
}

impl Rule {
    pub fn new(field: Field, op: Op, value: impl Into<String>) -> Self {
        Self {
            field,
            op,
            value: value.into(),
        }
    }
}

/// Whether every child must match, or any one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    All,
    Any,
}

impl Match {
    fn joiner(self) -> &'static str {
        match self {
            Self::All => " AND ",
            Self::Any => " OR ",
        }
    }

    /// What an empty group means.
    ///
    /// An empty "all" matches everything (there is nothing to fail); an empty
    /// "any" matches nothing (there is nothing to satisfy). Getting this
    /// backwards makes a half-built rule in the UI silently select the whole
    /// library.
    fn empty_value(self) -> &'static str {
        match self {
            Self::All => "1",
            Self::Any => "0",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Rule(Rule),
    Group(Group),
}

/// A set of rules joined by one operator, nestable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub matching: Match,
    #[serde(default)]
    pub nodes: Vec<Node>,
}

impl Group {
    pub fn all(nodes: Vec<Node>) -> Self {
        Self {
            matching: Match::All,
            nodes,
        }
    }

    pub fn any(nodes: Vec<Node>) -> Self {
        Self {
            matching: Match::Any,
            nodes,
        }
    }
}

impl Default for Group {
    fn default() -> Self {
        Self::all(Vec::new())
    }
}

/// A complete smart playlist definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartRules {
    pub root: Group,
    /// Cap on how many tracks the playlist yields.
    pub limit: Option<u32>,
    pub order: Order,
}

impl Default for SmartRules {
    fn default() -> Self {
        Self {
            root: Group::default(),
            limit: None,
            order: Order::Title,
        }
    }
}

impl SmartRules {
    /// Parse a stored rule document.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(text)?)
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Whether any rule has actually been added.
    pub fn is_empty(&self) -> bool {
        self.root.nodes.is_empty()
    }

    /// Compile to a `WHERE` fragment and its bound parameters.
    ///
    /// `now` is the current unix time, used by the time-relative operators. It
    /// is passed in rather than read here so a test can pin it.
    pub fn to_sql(&self, now: i64) -> Compiled {
        let mut params = Vec::new();
        let where_clause = compile_group(&self.root, now, &mut params);

        Compiled {
            where_clause,
            params,
        }
    }
}

/// A rule set turned into SQL.
#[derive(Debug, Clone)]
pub struct Compiled {
    /// A boolean expression, safe to drop into a `WHERE`. Never empty.
    pub where_clause: String,
    pub params: Vec<rusqlite::types::Value>,
}

fn compile_group(group: &Group, now: i64, params: &mut Vec<rusqlite::types::Value>) -> String {
    if group.nodes.is_empty() {
        return group.matching.empty_value().to_owned();
    }

    let parts: Vec<String> = group
        .nodes
        .iter()
        .map(|node| match node {
            Node::Rule(rule) => compile_rule(rule, now, params),
            Node::Group(inner) => compile_group(inner, now, params),
        })
        .collect();

    format!("({})", parts.join(group.matching.joiner()))
}

fn compile_rule(rule: &Rule, now: i64, params: &mut Vec<rusqlite::types::Value>) -> String {
    use rusqlite::types::Value;

    // Genre is many-to-many, so it tests for the existence of a matching join
    // row rather than comparing a column on the track.
    if rule.field == Field::Genre {
        return compile_genre(rule, params);
    }

    let Some(column) = rule.field.column() else {
        // Unreachable: Genre is the only column-less field and it returned
        // above. Failing closed beats compiling a broken expression.
        return "0".to_owned();
    };

    match rule.op {
        Op::IsSet => return format!("({column} IS NOT NULL AND {column} != '')"),
        Op::IsNotSet => return format!("({column} IS NULL OR {column} = '')"),
        _ => {}
    }

    match rule.field.kind() {
        Kind::Text => compile_text(column, rule, params),
        Kind::Number => compile_number(column, rule, params),
        Kind::Date => compile_date(column, rule, now, params),
        Kind::Flag => {
            let wanted = matches!(rule.value.trim(), "true" | "1" | "yes");
            params.push(Value::Integer(i64::from(wanted)));
            // A missing flag counts as false rather than as unknown.
            format!("(COALESCE({column}, 0) != 0) = (? != 0)")
        }
    }
}

fn compile_genre(rule: &Rule, params: &mut Vec<rusqlite::types::Value>) -> String {
    use rusqlite::types::Value;

    let exists = |predicate: &str| {
        format!(
            "EXISTS (SELECT 1 FROM track_genres tg
                       JOIN genres g ON g.id = tg.genre_id
                      WHERE tg.track_id = t.id AND {predicate})"
        )
    };

    match rule.op {
        Op::IsSet => exists("1"),
        Op::IsNotSet => format!("NOT {}", exists("1")),
        Op::Is => {
            params.push(Value::Text(rule.value.to_lowercase()));
            exists("lower(g.name) = ?")
        }
        Op::IsNot => {
            params.push(Value::Text(rule.value.to_lowercase()));
            format!("NOT {}", exists("lower(g.name) = ?"))
        }
        Op::DoesNotContain => {
            params.push(Value::Text(format!("%{}%", like_escape(&rule.value))));
            format!("NOT {}", exists(r"lower(g.name) LIKE ? ESCAPE '\'"))
        }
        Op::StartsWith => {
            params.push(Value::Text(format!("{}%", like_escape(&rule.value))));
            exists(r"lower(g.name) LIKE ? ESCAPE '\'")
        }
        Op::EndsWith => {
            params.push(Value::Text(format!("%{}", like_escape(&rule.value))));
            exists(r"lower(g.name) LIKE ? ESCAPE '\'")
        }
        // Contains, and anything numeric that reached a text field.
        _ => {
            params.push(Value::Text(format!("%{}%", like_escape(&rule.value))));
            exists(r"lower(g.name) LIKE ? ESCAPE '\'")
        }
    }
}

fn compile_text(column: &str, rule: &Rule, params: &mut Vec<rusqlite::types::Value>) -> String {
    use rusqlite::types::Value;

    let lowered = format!("lower(COALESCE({column}, ''))");

    match rule.op {
        Op::Is => {
            params.push(Value::Text(rule.value.to_lowercase()));
            format!("{lowered} = ?")
        }
        Op::IsNot => {
            params.push(Value::Text(rule.value.to_lowercase()));
            format!("{lowered} != ?")
        }
        Op::DoesNotContain => {
            params.push(Value::Text(format!("%{}%", like_escape(&rule.value))));
            format!(r"{lowered} NOT LIKE ? ESCAPE '\'")
        }
        Op::StartsWith => {
            params.push(Value::Text(format!("{}%", like_escape(&rule.value))));
            format!(r"{lowered} LIKE ? ESCAPE '\'")
        }
        Op::EndsWith => {
            params.push(Value::Text(format!("%{}", like_escape(&rule.value))));
            format!(r"{lowered} LIKE ? ESCAPE '\'")
        }
        // Contains, plus the numeric operators if a UI ever offers one here.
        _ => {
            params.push(Value::Text(format!("%{}%", like_escape(&rule.value))));
            format!(r"{lowered} LIKE ? ESCAPE '\'")
        }
    }
}

fn compile_number(column: &str, rule: &Rule, params: &mut Vec<rusqlite::types::Value>) -> String {
    use rusqlite::types::Value;

    let Some(number) = parse_number(&rule.value) else {
        // A half-typed rule should match nothing rather than everything, so
        // the preview count in the builder reads 0 while it is incomplete
        // instead of showing the entire library.
        return "0".to_owned();
    };

    // Durations are entered in seconds and stored in milliseconds.
    let scaled = if rule.field == Field::Duration {
        number * 1000.0
    } else {
        number
    };

    params.push(Value::Real(scaled));

    match rule.op {
        Op::IsNot => format!("COALESCE({column}, 0) != ?"),
        Op::GreaterThan => format!("COALESCE({column}, 0) > ?"),
        Op::LessThan => format!("COALESCE({column}, 0) < ?"),
        _ => format!("COALESCE({column}, 0) = ?"),
    }
}

fn compile_date(
    column: &str,
    rule: &Rule,
    now: i64,
    params: &mut Vec<rusqlite::types::Value>,
) -> String {
    use rusqlite::types::Value;

    let Some(days) = parse_number(&rule.value) else {
        return "0".to_owned();
    };

    let cutoff = now - (days.max(0.0) * 86_400.0) as i64;
    params.push(Value::Integer(cutoff));

    match rule.op {
        // "Not in the last N days" has to include tracks with no date at all —
        // a track never played is emphatically not one played this week, and
        // a plain `<` comparison against NULL would drop it silently.
        Op::NotInTheLast => format!("({column} IS NULL OR {column} < ?)"),
        _ => format!("{column} >= ?"),
    }
}

/// Escape the wildcards in a `LIKE` pattern, and lowercase it.
///
/// Without this, searching for `50%` matches every track whose title starts
/// with `50`, and searching for `_` matches everything.
fn like_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());

    for character in value.to_lowercase().chars() {
        if matches!(character, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }

    out
}

/// Read a number out of whatever the user typed.
fn parse_number(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(nodes: Vec<Node>) -> SmartRules {
        SmartRules {
            root: Group::all(nodes),
            ..SmartRules::default()
        }
    }

    fn rule(field: Field, op: Op, value: &str) -> Node {
        Node::Rule(Rule::new(field, op, value))
    }

    #[test]
    fn a_rule_set_survives_a_round_trip_through_json() {
        let original = SmartRules {
            root: Group::any(vec![
                rule(Field::Artist, Op::Is, "Boards of Canada"),
                Node::Group(Group::all(vec![
                    rule(Field::Genre, Op::Contains, "ambient"),
                    rule(Field::PlayCount, Op::GreaterThan, "3"),
                ])),
            ]),
            limit: Some(50),
            order: Order::LastPlayed,
        };

        let json = original.to_json().unwrap();
        let parsed = SmartRules::from_json(&json).unwrap();

        assert_eq!(parsed, original);
    }

    /// Nothing the user types may reach the SQL text.
    #[test]
    fn values_are_bound_rather_than_interpolated() {
        let hostile = "'; DROP TABLE tracks; --";
        let compiled = rules(vec![rule(Field::Title, Op::Contains, hostile)]).to_sql(0);

        assert!(
            !compiled.where_clause.contains("DROP"),
            "the value reached the SQL: {}",
            compiled.where_clause
        );
        assert_eq!(compiled.params.len(), 1);
    }

    /// A title containing a percent sign should search for a percent sign.
    #[test]
    fn like_wildcards_in_a_value_are_escaped() {
        assert_eq!(like_escape("50%"), r"50\%");
        assert_eq!(like_escape("a_b"), r"a\_b");
        assert_eq!(like_escape(r"back\slash"), r"back\\slash");
        // And it lowercases, because the comparison is against `lower(...)`.
        assert_eq!(like_escape("MiXeD"), "mixed");
    }

    /// An empty "all" matches everything and an empty "any" matches nothing.
    /// Getting this backwards makes a half-built rule select the whole library.
    #[test]
    fn empty_groups_have_the_right_identity() {
        let all = SmartRules {
            root: Group::all(vec![]),
            ..SmartRules::default()
        };
        assert_eq!(all.to_sql(0).where_clause, "1");

        let any = SmartRules {
            root: Group::any(vec![]),
            ..SmartRules::default()
        };
        assert_eq!(any.to_sql(0).where_clause, "0");
    }

    /// A rule whose value has not been filled in yet must match nothing, so
    /// the builder's live count reads 0 rather than the whole library.
    #[test]
    fn an_incomplete_numeric_rule_matches_nothing() {
        for value in ["", "   ", "not a number"] {
            let compiled = rules(vec![rule(Field::PlayCount, Op::GreaterThan, value)]).to_sql(0);
            assert_eq!(
                compiled.where_clause, "(0)",
                "an unparseable value produced {}",
                compiled.where_clause
            );
            assert!(compiled.params.is_empty());
        }
    }

    #[test]
    fn nesting_produces_parenthesised_groups() {
        let compiled = SmartRules {
            root: Group::all(vec![
                rule(Field::Artist, Op::Is, "a"),
                Node::Group(Group::any(vec![
                    rule(Field::Year, Op::GreaterThan, "1990"),
                    rule(Field::Year, Op::LessThan, "1980"),
                ])),
            ]),
            ..SmartRules::default()
        }
        .to_sql(0);

        assert!(compiled.where_clause.contains(" AND "));
        assert!(compiled.where_clause.contains(" OR "));
        // Three values bound, in the order they appear.
        assert_eq!(compiled.params.len(), 3);
    }

    /// Durations are typed in seconds and stored in milliseconds.
    #[test]
    fn a_duration_rule_is_converted_to_milliseconds() {
        let compiled = rules(vec![rule(Field::Duration, Op::GreaterThan, "180")]).to_sql(0);

        match &compiled.params[0] {
            rusqlite::types::Value::Real(value) => assert_eq!(*value, 180_000.0),
            other => panic!("expected a real, got {other:?}"),
        }
    }

    #[test]
    fn a_relative_date_becomes_an_absolute_cutoff() {
        let now = 1_000_000_000;
        let compiled = rules(vec![rule(Field::DateAdded, Op::InTheLast, "7")]).to_sql(now);

        match &compiled.params[0] {
            rusqlite::types::Value::Integer(value) => {
                assert_eq!(*value, now - 7 * 86_400);
            }
            other => panic!("expected an integer, got {other:?}"),
        }
    }

    /// A track that has never been played is not one played in the last week.
    #[test]
    fn not_in_the_last_includes_tracks_with_no_date() {
        let compiled = rules(vec![rule(Field::LastPlayed, Op::NotInTheLast, "30")]).to_sql(0);

        assert!(
            compiled.where_clause.contains("IS NULL"),
            "never-played tracks would be dropped: {}",
            compiled.where_clause
        );
    }

    #[test]
    fn genre_compiles_to_a_subquery_rather_than_a_column() {
        let compiled = rules(vec![rule(Field::Genre, Op::Contains, "jazz")]).to_sql(0);

        assert!(compiled.where_clause.contains("EXISTS"));
        assert!(compiled.where_clause.contains("track_genres"));
    }

    #[test]
    fn negated_genre_rules_are_wrapped_in_a_not() {
        for op in [Op::IsNot, Op::DoesNotContain] {
            let compiled = rules(vec![rule(Field::Genre, op, "jazz")]).to_sql(0);
            assert!(
                compiled.where_clause.contains("NOT EXISTS"),
                "{op:?} did not negate: {}",
                compiled.where_clause
            );
        }
    }

    /// Every field must compile to something, at every operator its kind
    /// offers — a combination that produced broken SQL would only show up when
    /// a user happened to build that exact rule.
    #[test]
    fn every_field_and_operator_pairing_compiles() {
        for field in Field::ALL {
            for op in Op::for_kind(field.kind()) {
                let compiled = rules(vec![rule(field, *op, "7")]).to_sql(0);

                assert!(
                    !compiled.where_clause.is_empty(),
                    "{field:?} {op:?} compiled to nothing"
                );
                // Balanced parentheses, or the surrounding query breaks.
                let opens = compiled.where_clause.matches('(').count();
                let closes = compiled.where_clause.matches(')').count();
                assert_eq!(
                    opens, closes,
                    "{field:?} {op:?} produced unbalanced parentheses: {}",
                    compiled.where_clause
                );
            }
        }
    }

    /// The operator list a field offers has to make sense for it.
    #[test]
    fn fields_only_offer_operators_that_suit_them() {
        assert!(Op::for_kind(Kind::Text).contains(&Op::Contains));
        assert!(!Op::for_kind(Kind::Number).contains(&Op::Contains));
        assert!(!Op::for_kind(Kind::Text).contains(&Op::GreaterThan));
        assert!(Op::for_kind(Kind::Date).contains(&Op::InTheLast));

        for kind in [Kind::Text, Kind::Number, Kind::Date, Kind::Flag] {
            assert!(
                !Op::for_kind(kind).is_empty(),
                "{kind:?} offers no operators"
            );
        }
    }

    #[test]
    fn value_less_operators_bind_no_parameters() {
        for op in [Op::IsSet, Op::IsNotSet] {
            assert!(!op.takes_a_value());

            let compiled = rules(vec![rule(Field::Album, op, "ignored")]).to_sql(0);
            assert!(
                compiled.params.is_empty(),
                "{op:?} bound a parameter it should not read"
            );
        }
    }

    #[test]
    fn every_field_and_operator_has_a_label() {
        for field in Field::ALL {
            assert!(!field.label().is_empty(), "{field:?} needs a label");
        }
        for kind in [Kind::Text, Kind::Number, Kind::Date, Kind::Flag] {
            for op in Op::for_kind(kind) {
                assert!(!op.label().is_empty(), "{op:?} needs a label");
            }
        }
    }
}
