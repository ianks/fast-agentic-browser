//! Plain-words secret names (`{{card number}}`, `{{GitHub 2FA code}}`,
//! `{{Stripe test secret key}}`) → which field of which kind of item. Models
//! name secrets in words, never by vault path, so the words are the interface.

/// What a name asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Login(LoginField),
    /// A strong new password, generated once per site.
    NewPassword,
    /// An email address: the site login's username when it is one, else the identity's.
    Email,
    Card(CardField),
    Identity(IdField),
    /// Anything else: an item and a field named by the words.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginField {
    Username,
    Password,
    Otp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardField {
    Number,
    Exp,
    ExpMonth,
    ExpYear,
    Csc,
    Name,
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdField {
    Name,
    Given,
    Middle,
    Family,
    Tel,
    Street,
    Line2,
    City,
    Region,
    Postal,
    Country,
    Org,
    Birthday,
    Username,
}

impl Want {
    /// The web's standard autofill name (HTML `autocomplete`), used by helpers.
    pub fn token(self) -> &'static str {
        use {CardField as C, IdField as I, LoginField as L};
        match self {
            Want::Login(L::Username) => "username",
            Want::Login(L::Password) => "current-password",
            Want::Login(L::Otp) => "one-time-code",
            Want::NewPassword => "new-password",
            Want::Email => "email",
            Want::Card(C::Number) => "cc-number",
            Want::Card(C::Exp) => "cc-exp",
            Want::Card(C::ExpMonth) => "cc-exp-month",
            Want::Card(C::ExpYear) => "cc-exp-year",
            Want::Card(C::Csc) => "cc-csc",
            Want::Card(C::Name) => "cc-name",
            Want::Card(C::Type) => "cc-type",
            Want::Identity(I::Name) => "name",
            Want::Identity(I::Given) => "given-name",
            Want::Identity(I::Middle) => "additional-name",
            Want::Identity(I::Family) => "family-name",
            Want::Identity(I::Tel) => "tel",
            Want::Identity(I::Street) => "address-line1",
            Want::Identity(I::Line2) => "address-line2",
            Want::Identity(I::City) => "address-level2",
            Want::Identity(I::Region) => "address-level1",
            Want::Identity(I::Postal) => "postal-code",
            Want::Identity(I::Country) => "country-name",
            Want::Identity(I::Org) => "organization",
            Want::Identity(I::Birthday) => "bday",
            Want::Identity(I::Username) => "username",
            Want::Custom => "custom",
        }
    }

    /// Values that never leave fab: masked on the page listing and scrubbed from output.
    pub fn concealed(self) -> bool {
        matches!(self, Want::Login(LoginField::Password | LoginField::Otp) | Want::NewPassword | Want::Card(CardField::Number | CardField::Csc))
    }
}

/// A parsed name: what it asks for, plus the leftover words that pick the
/// item ("Visa", "GitHub", "work").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub want: Want,
    pub hints: Vec<String>,
    /// All words of the name (for custom lookups).
    pub words: Vec<String>,
}

/// Longest phrases first; the first phrase found in the name wins.
const PHRASES: &[(&str, Want)] = {
    use {CardField as C, IdField as I, LoginField as L, Want as W};
    &[
        ("card verification code", W::Card(C::Csc)),
        ("credit card number", W::Card(C::Number)),
        ("card security code", W::Card(C::Csc)),
        ("new strong password", W::NewPassword),
        ("one time password", W::Login(L::Otp)),
        ("two factor code", W::Login(L::Otp)),
        ("address line 1", W::Identity(I::Street)),
        ("address line 2", W::Identity(I::Line2)),
        ("card holder name", W::Card(C::Name)),
        ("date of birth", W::Identity(I::Birthday)),
        ("one time code", W::Login(L::Otp)),
        ("sign in code", W::Login(L::Otp)),
        ("expiration month", W::Card(C::ExpMonth)),
        ("expiration year", W::Card(C::ExpYear)),
        ("expiration date", W::Card(C::Exp)),
        ("verification number", W::Card(C::Csc)),
        ("verification code", W::Login(L::Otp)),
        ("authentication code", W::Login(L::Otp)),
        ("authenticator code", W::Login(L::Otp)),
        ("generated password", W::NewPassword),
        ("generate password", W::NewPassword),
        ("random password", W::NewPassword),
        ("strong password", W::NewPassword),
        ("new password", W::NewPassword),
        ("current password", W::Login(L::Password)),
        ("cardholder name", W::Card(C::Name)),
        ("name on card", W::Card(C::Name)),
        ("card number", W::Card(C::Number)),
        ("cc number", W::Card(C::Number)),
        ("card no", W::Card(C::Number)),
        ("card holder", W::Card(C::Name)),
        ("card type", W::Card(C::Type)),
        ("card brand", W::Card(C::Type)),
        ("security code", W::Card(C::Csc)),
        ("expiry month", W::Card(C::ExpMonth)),
        ("exp month", W::Card(C::ExpMonth)),
        ("expiry year", W::Card(C::ExpYear)),
        ("exp year", W::Card(C::ExpYear)),
        ("expiry date", W::Card(C::Exp)),
        ("exp date", W::Card(C::Exp)),
        ("valid thru", W::Card(C::Exp)),
        ("mm yy", W::Card(C::Exp)),
        ("2fa code", W::Login(L::Otp)),
        ("mfa code", W::Login(L::Otp)),
        ("otp code", W::Login(L::Otp)),
        ("auth code", W::Login(L::Otp)),
        ("login code", W::Login(L::Otp)),
        ("two factor", W::Login(L::Otp)),
        ("email address", W::Email),
        ("e mail", W::Email),
        ("user name", W::Login(L::Username)),
        ("user id", W::Login(L::Username)),
        ("account name", W::Login(L::Username)),
        ("full name", W::Identity(I::Name)),
        ("first name", W::Identity(I::Given)),
        ("given name", W::Identity(I::Given)),
        ("middle name", W::Identity(I::Middle)),
        ("middle initial", W::Identity(I::Middle)),
        ("last name", W::Identity(I::Family)),
        ("family name", W::Identity(I::Family)),
        ("phone number", W::Identity(I::Tel)),
        ("mobile number", W::Identity(I::Tel)),
        ("cell phone", W::Identity(I::Tel)),
        ("street address", W::Identity(I::Street)),
        ("zip code", W::Identity(I::Postal)),
        ("postal code", W::Identity(I::Postal)),
        ("post code", W::Identity(I::Postal)),
        ("birth date", W::Identity(I::Birthday)),
        ("cardholder", W::Card(C::Name)),
        ("expiration", W::Card(C::Exp)),
        ("expiry", W::Card(C::Exp)),
        ("expires", W::Card(C::Exp)),
        ("cvc2", W::Card(C::Csc)),
        ("cvc", W::Card(C::Csc)),
        ("cvv", W::Card(C::Csc)),
        ("csc", W::Card(C::Csc)),
        ("otp", W::Login(L::Otp)),
        ("totp", W::Login(L::Otp)),
        ("2fa", W::Login(L::Otp)),
        ("mfa", W::Login(L::Otp)),
        ("passcode", W::Login(L::Password)),
        ("password", W::Login(L::Password)),
        ("passwd", W::Login(L::Password)),
        ("username", W::Login(L::Username)),
        ("userid", W::Login(L::Username)),
        ("login", W::Login(L::Username)),
        ("email", W::Email),
        ("forename", W::Identity(I::Given)),
        ("surname", W::Identity(I::Family)),
        ("initial", W::Identity(I::Middle)),
        ("telephone", W::Identity(I::Tel)),
        ("phone", W::Identity(I::Tel)),
        ("mobile", W::Identity(I::Tel)),
        ("tel", W::Identity(I::Tel)),
        ("address", W::Identity(I::Street)),
        ("street", W::Identity(I::Street)),
        ("apartment", W::Identity(I::Line2)),
        ("city", W::Identity(I::City)),
        ("town", W::Identity(I::City)),
        ("state", W::Identity(I::Region)),
        ("province", W::Identity(I::Region)),
        ("region", W::Identity(I::Region)),
        ("county", W::Identity(I::Region)),
        ("zip", W::Identity(I::Postal)),
        ("postcode", W::Identity(I::Postal)),
        ("country", W::Identity(I::Country)),
        ("company", W::Identity(I::Org)),
        ("organization", W::Identity(I::Org)),
        ("organisation", W::Identity(I::Org)),
        ("birthday", W::Identity(I::Birthday)),
        ("dob", W::Identity(I::Birthday)),
        ("name", W::Identity(I::Name)),
        ("code", W::Login(L::Otp)),
    ]
};

/// Words that say where a value comes from, not which one it is.
const NOISE: &[&str] = &["the", "my", "your", "their", "users", "user's", "saved", "stored", "from", "in", "of", "for", "value", "pm", "vault", "use"];

/// Lowercase words, with `_ - . / :` and quotes as separators ("PASSWORD_MANAGER_CARD_NUMBER"
/// → "password manager card number").
fn words(s: &str) -> Vec<String> {
    let s = s.replace(['\'', '’'], "");
    let mut w: Vec<String> = s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    // "password manager" says where the value is, never what it is.
    while let Some(i) = w.windows(2).position(|p| p[0] == "password" && p[1] == "manager") {
        w.drain(i..i + 2);
    }
    // Split camelCase that survived ("cardNumber"): only when the original had no separators.
    if w.len() == 1 && s.chars().any(|c| c.is_uppercase()) && s.chars().any(|c| c.is_lowercase()) {
        let mut out = vec![];
        let mut cur = String::new();
        for c in s.chars().filter(|c| c.is_alphanumeric()) {
            if c.is_uppercase() && !cur.is_empty() && !cur.chars().last().is_some_and(char::is_uppercase) {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            cur.push(c);
        }
        out.push(cur.to_lowercase());
        return out;
    }
    w
}

pub fn parse(name: &str) -> Name {
    let all = words(name);
    let content: Vec<String> = all.iter().filter(|w| !NOISE.contains(&w.as_str())).cloned().collect();
    for (phrase, want) in PHRASES {
        let p: Vec<&str> = phrase.split(' ').collect();
        if let Some(at) = content.windows(p.len()).position(|w| w.iter().zip(&p).all(|(a, b)| a == b)) {
            let mut hints = content.clone();
            hints.drain(at..at + p.len());
            // "number" alone is a card number only next to a card word.
            return Name { want: *want, hints, words: content };
        }
    }
    let card = content.iter().any(|w| matches!(w.as_str(), "card" | "visa" | "mastercard" | "amex" | "discover"));
    if card && content.iter().any(|w| w == "number") {
        let hints = content.iter().filter(|w| *w != "number").cloned().collect();
        return Name { want: Want::Card(CardField::Number), hints, words: content };
    }
    Name { want: Want::Custom, hints: content.clone(), words: content }
}

/// The `{{…}}` placeholders in `s`, as (byte range, inner name).
pub fn placeholders(s: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(a) = s[from..].find("{{").map(|i| i + from) {
        let Some(b) = s[a + 2..].find("}}").map(|i| i + a + 2) else { break };
        let inner = s[a + 2..b].trim();
        if !inner.is_empty() && !inner.contains('{') {
            out.push((a..b + 2, inner));
        }
        from = b + 2;
    }
    out
}

pub fn has_placeholder(s: &str) -> bool {
    !placeholders(s).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use {CardField as C, IdField as I, LoginField as L};

    fn want(s: &str) -> Want {
        parse(s).want
    }

    #[test]
    fn plain_names() {
        assert_eq!(want("password"), Want::Login(L::Password));
        assert_eq!(want("new password"), Want::NewPassword);
        assert_eq!(want("one-time code"), Want::Login(L::Otp));
        assert_eq!(want("authenticator_code"), Want::Login(L::Otp));
        assert_eq!(want("GitHub 2FA code"), Want::Login(L::Otp));
        assert_eq!(want("card number"), Want::Card(C::Number));
        assert_eq!(want("PASSWORD_MANAGER_CARD_NUMBER"), Want::Card(C::Number));
        assert_eq!(want("PASSWORD_MANAGER_CARD_EXPIRY"), Want::Card(C::Exp));
        assert_eq!(want("visa_card_number"), Want::Card(C::Number));
        assert_eq!(want("Visa CVC"), Want::Card(C::Csc));
        assert_eq!(want("cardNumber"), Want::Card(C::Number));
        assert_eq!(want("shipping address full name"), Want::Identity(I::Name));
        assert_eq!(want("shipping address postal code"), Want::Identity(I::Postal));
        assert_eq!(want("ZIP code"), Want::Identity(I::Postal));
        assert_eq!(want("first_name"), Want::Identity(I::Given));
        assert_eq!(want("phone number"), Want::Identity(I::Tel));
        assert_eq!(want("email"), Want::Email);
        assert_eq!(want("username"), Want::Login(L::Username));
        assert_eq!(want("Stripe test secret key"), Want::Custom);
        assert_eq!(want("GitHub token"), Want::Custom);
    }

    #[test]
    fn hints_pick_the_item() {
        let n = parse("Personal Visa card number");
        assert_eq!(n.want, Want::Card(C::Number));
        assert_eq!(n.hints, vec!["personal", "visa"]);
        let n = parse("the user's GitHub password");
        assert_eq!(n.hints, vec!["github"]);
        assert_eq!(parse("Stripe test secret key").words, vec!["stripe", "test", "secret", "key"]);
        assert_eq!(want("code"), Want::Login(L::Otp));
    }

    #[test]
    fn finds_placeholders() {
        let p = placeholders("{{first name}} {{ last name }}");
        assert_eq!(p.iter().map(|(_, n)| *n).collect::<Vec<_>>(), vec!["first name", "last name"]);
        assert!(has_placeholder("{{password}}"));
        assert!(!has_placeholder("{{}}"));
        assert!(!has_placeholder("a { b } c"));
        assert!(!has_placeholder("{{unterminated"));
    }
}
