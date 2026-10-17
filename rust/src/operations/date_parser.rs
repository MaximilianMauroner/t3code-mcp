// Adapted from the V8 date parser. Copyright 2011 the V8 project authors.
// The license is in date_parser.LICENSE.
use chrono::{Duration, Local, NaiveDate, NaiveDateTime, TimeZone};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Keyword {
    Month(i32),
    Zone(i32),
    Z,
    Time,
    AmPm(i32),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Token {
    Number(i32, usize),
    Word(Keyword),
    Symbol(char),
    Space,
    Ignored,
    End,
}

struct Tokens {
    items: Vec<Token>,
    position: usize,
}

impl Tokens {
    fn new(text: &str) -> Self {
        let chars: Vec<_> = text.chars().take_while(|c| *c != '\0').collect();
        let mut items = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let start = i;
            let character = chars[i];
            let token = if character.is_ascii_digit() {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let number = chars[start..i]
                    .iter()
                    .skip_while(|c| **c == '0')
                    .take(9)
                    .fold(0, |n, c| n * 10 + c.to_digit(10).unwrap() as i32);
                Token::Number(number, i - start)
            } else if [':', '-', '+', '.', ')'].contains(&character) {
                i += 1;
                Token::Symbol(character)
            } else if character >= 'A' && !word_space(character) {
                while i < chars.len() && chars[i] >= 'A' && !word_space(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i]
                    .iter()
                    .map(char::to_ascii_lowercase)
                    .collect();
                Token::Word(keyword(&word))
            } else if super::audit_log::js_trim(&character.to_string()).is_empty() {
                i += 1;
                Token::Space
            } else if character == '(' {
                let mut balance = 1;
                i += 1;
                while i < chars.len() && balance > 0 {
                    if chars[i] == '(' {
                        balance += 1;
                    }
                    if chars[i] == ')' {
                        balance -= 1;
                    }
                    i += 1;
                }
                Token::Ignored
            } else {
                i += 1;
                Token::Ignored
            };
            items.push(token);
        }
        Self { items, position: 0 }
    }

    fn peek(&self) -> Token {
        self.items.get(self.position).copied().unwrap_or(Token::End)
    }
    fn next(&mut self) -> Token {
        let token = self.peek();
        self.position += usize::from(token != Token::End);
        token
    }
    fn skip(&mut self, symbol: char) -> bool {
        if self.peek() != Token::Symbol(symbol) {
            return false;
        }
        self.next();
        true
    }
    fn fixed(&mut self, length: usize, min: i32, max: i32) -> Option<i32> {
        match self.peek() {
            Token::Number(number, actual) if length == actual && (min..=max).contains(&number) => {
                self.next();
                Some(number)
            }
            _ => None,
        }
    }
}

fn word_space(character: char) -> bool {
    !matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
        && super::audit_log::js_trim(&character.to_string()).is_empty()
}

fn keyword(word: &str) -> Keyword {
    let months = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    if let Some(index) = months.iter().position(|month| word.starts_with(month)) {
        return Keyword::Month(index as i32 + 1);
    }
    match word {
        "am" => Keyword::AmPm(0),
        "pm" => Keyword::AmPm(12),
        "z" => Keyword::Z,
        "t" => Keyword::Time,
        "ut" | "utc" | "gmt" => Keyword::Zone(0),
        "cdt" | "est" => Keyword::Zone(-5),
        "cst" | "mdt" => Keyword::Zone(-6),
        "edt" => Keyword::Zone(-4),
        "mst" | "pdt" => Keyword::Zone(-7),
        "pst" => Keyword::Zone(-8),
        _ => Keyword::Other,
    }
}

#[derive(Default)]
struct Date {
    parts: Vec<i32>,
    named_month: Option<i32>,
    iso: bool,
}

impl Date {
    fn add(&mut self, number: i32) -> Option<()> {
        if self.parts.len() >= 3 {
            return None;
        }
        self.parts.push(number);
        Some(())
    }

    fn finish(mut self) -> Option<NaiveDate> {
        if self.parts.is_empty() {
            return None;
        }
        self.parts.resize(3, 1);
        let (mut year, month, day) = if let Some(month) = self.named_month {
            if !(1..=31).contains(&self.parts[0]) {
                (self.parts[0], month, self.parts[1])
            } else {
                (self.parts[1], month, self.parts[0])
            }
        } else if self.iso || !(1..=31).contains(&self.parts[0]) {
            (self.parts[0], self.parts[1], self.parts[2])
        } else {
            (self.parts[2], self.parts[0], self.parts[1])
        };
        if !self.iso {
            if (0..=49).contains(&year) {
                year += 2000;
            } else if (50..=99).contains(&year) {
                year += 1900;
            }
        }
        if !(1..=31).contains(&day) {
            return None;
        }
        NaiveDate::from_ymd_opt(year, month.try_into().ok()?, 1)?
            .checked_add_signed(Duration::days(i64::from(day) - 1))
    }
}

#[derive(Default)]
struct Time {
    parts: Vec<i32>,
    hour_offset: Option<i32>,
}

impl Time {
    fn add(&mut self, number: i32) -> Option<()> {
        if self.parts.len() >= 4 {
            return None;
        }
        self.parts.push(number);
        Some(())
    }
    fn final_part(&mut self, number: i32) -> Option<()> {
        self.add(number)?;
        self.parts.resize(4, 0);
        Some(())
    }
    fn expects(&self, number: i32) -> bool {
        match self.parts.len() {
            1 | 2 => (0..=59).contains(&number),
            3 => (0..=999).contains(&number),
            _ => false,
        }
    }
    fn finish(mut self, date: NaiveDate) -> Option<NaiveDateTime> {
        self.parts.resize(4, 0);
        let [mut hour, minute, second, millis] = self.parts[..] else {
            return None;
        };
        if let Some(offset) = self.hour_offset {
            if !(0..=12).contains(&hour) {
                return None;
            }
            hour = hour % 12 + offset;
        }
        if hour == 24 && minute == 0 && second == 0 && millis == 0 {
            return date.succ_opt()?.and_hms_opt(0, 0, 0);
        }
        if !(0..=23).contains(&hour)
            || !(0..=59).contains(&minute)
            || !(0..=59).contains(&second)
            || !(0..=999).contains(&millis)
        {
            return None;
        }
        date.and_hms_milli_opt(hour as u32, minute as u32, second as u32, millis as u32)
    }
}

#[derive(Default)]
struct Zone {
    sign: Option<i32>,
    hour: Option<i32>,
    minute: Option<i32>,
}

impl Zone {
    fn set(&mut self, hours: i32) {
        self.sign = Some(if hours < 0 { -1 } else { 1 });
        self.hour = Some(hours.abs());
        self.minute = Some(0);
    }
    fn is_utc(&self) -> bool {
        self.hour == Some(0) && self.minute == Some(0)
    }
    fn expects(&self, number: i32) -> bool {
        self.hour.is_some() && self.minute.is_none() && (0..=59).contains(&number)
    }
    fn finish(self, date: NaiveDateTime) -> Option<i64> {
        if let Some(sign) = self.sign {
            let seconds =
                i64::from(self.hour.unwrap_or(0)) * 3600 + i64::from(self.minute.unwrap_or(0)) * 60;
            date.and_utc()
                .timestamp_millis()
                .checked_sub(i64::from(sign) * seconds * 1000)
        } else {
            match Local.from_local_datetime(&date) {
                chrono::LocalResult::Single(time) => Some(time.timestamp_millis()),
                chrono::LocalResult::Ambiguous(first, second) => {
                    Some(first.timestamp_millis().min(second.timestamp_millis()))
                }
                chrono::LocalResult::None => {
                    let before = Local
                        .from_local_datetime(&date.checked_sub_signed(Duration::days(1))?)
                        .earliest()?;
                    Some(
                        date.and_utc().timestamp_millis()
                            - i64::from(before.offset().local_minus_utc()) * 1000,
                    )
                }
            }
        }
    }
}

fn milliseconds(number: i32, length: usize) -> i32 {
    match length {
        1 => number * 100,
        2 => number * 10,
        _ => number / 10_i32.pow(length.min(9) as u32 - 3),
    }
}

fn iso(
    tokens: &mut Tokens,
    date: &mut Date,
    time: &mut Time,
    zone: &mut Zone,
) -> Result<Token, ()> {
    if let Token::Symbol(sign @ ('+' | '-')) = tokens.peek() {
        tokens.next();
        let Some(year) = tokens.fixed(6, 0, 999999) else {
            return Ok(Token::Symbol(sign));
        };
        if sign == '-' && year == 0 {
            return Ok(Token::Symbol(sign));
        }
        date.add(year * if sign == '-' { -1 } else { 1 })
            .ok_or(())?;
    } else if let Some(year) = tokens.fixed(4, 0, 9999) {
        date.add(year).ok_or(())?;
    } else {
        return Ok(tokens.next());
    }
    if tokens.skip('-') {
        let Some(month) = tokens.fixed(2, 1, 12) else {
            return Ok(tokens.next());
        };
        date.add(month).ok_or(())?;
        if tokens.skip('-') {
            let Some(day) = tokens.fixed(2, 1, 31) else {
                return Ok(tokens.next());
            };
            date.add(day).ok_or(())?;
        }
    }
    if tokens.peek() == Token::Word(Keyword::Time) {
        tokens.next();
        let hour = tokens.fixed(2, 0, 24).ok_or(())?;
        time.add(hour).ok_or(())?;
        if !tokens.skip(':') {
            return Err(());
        }
        let minute = tokens
            .fixed(2, 0, if hour == 24 { 0 } else { 59 })
            .ok_or(())?;
        time.add(minute).ok_or(())?;
        if tokens.skip(':') {
            let second = tokens
                .fixed(2, 0, if hour == 24 { 0 } else { 59 })
                .ok_or(())?;
            time.add(second).ok_or(())?;
            if tokens.skip('.') {
                let Token::Number(number, length) = tokens.next() else {
                    return Err(());
                };
                if hour == 24 && number > 0 {
                    return Err(());
                }
                time.add(milliseconds(number, length)).ok_or(())?;
            }
        }
        match tokens.peek() {
            Token::Word(Keyword::Z) => {
                tokens.next();
                zone.set(0);
            }
            Token::Symbol(sign @ ('+' | '-')) => {
                tokens.next();
                zone.sign = Some(if sign == '-' { -1 } else { 1 });
                if let Some(number) = tokens.fixed(4, 0, 2359) {
                    if number % 100 > 59 {
                        return Err(());
                    }
                    zone.hour = Some(number / 100);
                    zone.minute = Some(number % 100);
                } else {
                    zone.hour = Some(tokens.fixed(2, 0, 23).ok_or(())?);
                    if !tokens.skip(':') {
                        return Err(());
                    }
                    zone.minute = Some(tokens.fixed(2, 0, 59).ok_or(())?);
                }
            }
            _ => (),
        }
        if tokens.peek() != Token::End {
            return Err(());
        }
    } else if tokens.peek() != Token::End {
        return Ok(tokens.next());
    }
    if zone.hour.is_none() && time.parts.is_empty() {
        zone.set(0);
    }
    date.iso = true;
    Ok(Token::End)
}

pub(crate) fn parse_timestamp(text: &str) -> Option<i64> {
    let mut tokens = Tokens::new(text);
    let mut date = Date::default();
    let mut time = Time::default();
    let mut zone = Zone::default();
    let mut token = iso(&mut tokens, &mut date, &mut time, &mut zone).ok()?;
    let mut has_number = !date.parts.is_empty();
    while token != Token::End {
        match token {
            Token::Number(number, _) => {
                has_number = true;
                if tokens.skip(':') {
                    if tokens.skip(':') {
                        if !time.parts.is_empty() {
                            return None;
                        }
                        time.add(number)?;
                        time.add(0)?;
                    } else {
                        time.add(number)?;
                        tokens.skip('.');
                    }
                } else if tokens.skip('.') && time.expects(number) {
                    time.add(number)?;
                    let Token::Number(value, length) = tokens.next() else {
                        return None;
                    };
                    time.final_part(milliseconds(value, length))?;
                } else if zone.expects(number) {
                    zone.minute = Some(number);
                } else if time.expects(number) {
                    time.final_part(number)?;
                    if !matches!(
                        tokens.peek(),
                        Token::End
                            | Token::Space
                            | Token::Word(Keyword::Z)
                            | Token::Symbol('+' | '-')
                    ) {
                        return None;
                    }
                } else {
                    date.add(number)?;
                    tokens.skip('-');
                }
            }
            Token::Word(keyword) => match keyword {
                Keyword::AmPm(offset) if !time.parts.is_empty() => time.hour_offset = Some(offset),
                Keyword::Month(month) => {
                    date.named_month = Some(month);
                    tokens.skip('-');
                }
                Keyword::Zone(offset) if has_number => zone.set(offset),
                Keyword::Z if has_number => zone.set(0),
                _ => {
                    if has_number || matches!(tokens.peek(), Token::Number(..)) {
                        return None;
                    }
                }
            },
            Token::Symbol(sign @ ('+' | '-')) if zone.is_utc() || !time.parts.is_empty() => {
                zone.sign = Some(if sign == '-' { -1 } else { 1 });
                let (number, length) = match tokens.peek() {
                    Token::Number(number, length) => {
                        tokens.next();
                        (number, length)
                    }
                    _ => (0, 0),
                };
                has_number = true;
                if tokens.peek() == Token::Symbol(':') {
                    zone.hour = Some(number);
                    zone.minute = None;
                } else if (1..=2).contains(&length) {
                    zone.hour = Some(number);
                    zone.minute = Some(0);
                } else if (3..=4).contains(&length) {
                    zone.hour = Some(number / 100);
                    zone.minute = Some(number % 100);
                } else {
                    return None;
                }
            }
            Token::Symbol('+' | '-' | ')') if has_number => return None,
            _ => (),
        }
        token = tokens.next();
    }
    let result = zone.finish(time.finish(date.finish()?)?)?;
    (result.abs() <= 8_640_000_000_000_000).then_some(result)
}
