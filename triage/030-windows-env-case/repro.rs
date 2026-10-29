use sandlang::{Capability, RulePolicy};

fn main() {
    let policy = RulePolicy::new().allow_env("*").deny_env("AWS_SECRET_ACCESS_KEY");
    for name in ["AWS_SECRET_ACCESS_KEY", "aws_secret_access_key", "Aws_Secret_Access_Key"] {
        println!("{name}: {:?}", policy.check(&Capability::Env(name.into())));
    }
}
