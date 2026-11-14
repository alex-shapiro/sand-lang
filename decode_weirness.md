# Notes on Decode Weirdness

- The JSON encoder should be called `JsonEncoder`, not `TextEncoder`, in keeping with AGENTS.md symbol naming reqs. Epistemic confidence: 100%.

- `json.Json` type is a normal type, and my prior is that there is no reason why it should be using a special snowflake TreeEncoder/TreeDecoder instead of the standard JSON encoder. Why it it? Epistemic confidence: 50%

- field visibility should not restrict `Decode`. In the absence of another mitigating factor (e.g. explicit init), a struct whose fields implement `Decode` should derive `Decode` regardless of their ACL. In general, the policy moving forward must be that a struct's field ACLs have no impact on whether a trait can be derived for that struct. Gemini's explanation is that the issue is in the `Checker`; I would like to know if the restriction is that the checker has no access to the fields, or whether the issue is that the checker chooses to limit itself solely because of prior belief in a policy. In either case, we would need a change, but the scope of those changes may differ. Epistemic confidence: 95%

- field defaults should not restrict `Decode` from being derived. A struct that contains fields with a default value should still derive `Decode` with the native fast path, in the same way that a struct whose fields have default values still derives a default native constructor. Epistemic confidence: 70%.

- The phrases "derived" and "synthesized" appear to be used interchangeably with no rhyme or reason. Pick one and stick with it universally. Epistemic confidence: 100%.

- I'm finding it a bit hard to follow, but AFAICT Sand appears to be checking a codec's native status at runtime in `Checker::codec_is_native`. That seems like a value which could easily be computed during compilation and then avoided at runtime. Not sure if it makes a big difference, but it seems like we should be frontloading as much of the computation to the compiler so that at runtime, Encode and Decode can run as quickly as possible. Epistemic confidence: 30%.

- The `Schema` type seems to me like it is probably duplicative in some way of other "understandings" of a type in the Sand VM. What are the alternatives and what are their shortcomings that make it either necessary or desirable?

- I don't understand
