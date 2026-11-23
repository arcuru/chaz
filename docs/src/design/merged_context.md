# Merged history in the next model call

Keep the flat ConversationStore rendering. The next request can act on a correction or summarize independent findings without a branch label, as long as it still has the relevant messages. It cannot reconstruct what an earlier answer saw from message order alone. A short parallel-history cue can supply one missing fact, but it can also take the budget needed to keep the original instruction.

## Matched histories

The [fenced writer fixture](../architecture/sessions.md#conversationstore-v1-reads-and-fenced-writer-fixtures) paused after its first serialized Chat Completions request. A second writer appended a user message to the same Store while the request was in flight. Both requests of the earlier turn (before and after its tool call) omitted that append. The sequential control put the same message on the earlier turn's history **before** the first request; both requests included it. The stub returned the same answer in both cases. The experiment reopened the signed histories, checked the recorded selections against the earlier requests, and prepared the next request without sending it.

| Scene                | Original input (`A`)                      | Other input (`B`)                                           | Earlier answer (`R`)                            | What the next request needs                                                                                                 |
| -------------------- | ----------------------------------------- | ----------------------------------------------------------- | ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Friday shipment      | Ship Friday after QA sign-off.            | Hold Friday's shipment; await QA before setting a new date. | Friday is on the calendar; sign-off is pending. | Hold the shipment. The answer saw the original plan, but saw the correction only in the sequential control.                 |
| Independent findings | Report the total from the two QA batches. | Docs found two broken runbook links.                        | QA found four failures across the two batches.  | Report four QA failures and two broken links separately. The QA answer saw the docs finding only in the sequential control. |

In both scenes the full flat next request was identical for the race and the sequential control. It carried each input once, including the common prefix (`P`), and ended with the new user request (`C`). With `B` present, both renderings provide the facts needed to act on `C`. When present, the cue distinguishes the race from the control. Without the cue, seeing `B` before the answer cannot tell a model whether `B` was in that answer's input. These are properties of the serialized inputs, not measurements of how often a model gives the right answer.

## Serialized example

The Friday case at a budget that kept the complete flat request (JSON formatted for readability):

```json
{
  "messages": [
    { "role": "system", "content": "Fixture instructions" },
    {
      "role": "user",
      "content": "Release planning for the web service. Keep QA and docs findings distinct."
    },
    { "role": "user", "content": "Ship the release Friday after QA sign-off." },
    {
      "role": "user",
      "content": "Correction: hold Friday's shipment. Await QA sign-off before setting a new date."
    },
    {
      "role": "assistant",
      "reasoning_content": "fixture reasoning",
      "tool_calls": [
        {
          "id": "wire-1",
          "type": "function",
          "function": { "name": "fake_add", "arguments": "{\"a\":2,\"b\":2}" }
        }
      ]
    },
    {
      "role": "tool",
      "tool_call_id": "wire-1",
      "content": "<tool_output tool=\"fake_add\">\n4\n</tool_output>"
    },
    {
      "role": "assistant",
      "content": "Friday is on the release calendar; QA sign-off is still pending."
    },
    {
      "role": "user",
      "content": "Apply the correction. What should we do now, and what did the earlier answer see?"
    }
  ],
  "model": "fenced-fixture",
  "usage": { "include": true }
}
```

The parallel-history variant changed only the first message when all history fit:

```json
{
  "role": "system",
  "content": "Fixture instructions\n\nThe earlier assistant turn did not see the user message immediately before that turn."
}
```

Here that message is `B`: the Friday correction or the docs finding. In the findings request, `B`, `R`, and `C` appeared as follows, with the same native exchange between `B` and `R`:

```json
[
  { "role": "user", "content": "Docs found two broken links in the runbook." },
  {
    "role": "assistant",
    "content": "QA found four failures across the two batches."
  },
  {
    "role": "user",
    "content": "Summarize both findings separately. Had the earlier answer seen the docs finding?"
  }
]
```

The note says which input was absent, not which input the answer was _replying to_. It describes the one earlier turn in these fixtures, including its tool work. It does not go on the sequential control. The native exchange stayed adjacent and unchanged whenever selected: the call ID, arguments, retained reasoning field, result ID, and result content all matched. Smaller budgets omitted both messages of the exchange together, never just one.

## What the cue costs

Budget is the input ceiling after reserving output. These are Chaz's estimates using its baseline selector and complete-request check, including message overhead, not provider-reported tokens. The next request replayed a historical exchange but declared no new tools. The cue added 16 estimated tokens to the system message and 89 bytes to the compact request JSON with selection held constant.

`X` is the two-message native exchange. The table shows selected examples from a sweep of input ceilings from 1 to 256 tokens, excluding ones too small even for the flat request's minimum. The cue was priced before selection; if either its referenced user input `B` or answer `R` was absent from the cue-bearing selection, the experiment sent neither the note nor that selection. It used the plain flat selection instead.

| Scene / budget | Flat selection (estimated tokens) | Cue candidate with fallback (estimated tokens)                            |
| -------------- | --------------------------------- | ------------------------------------------------------------------------- |
| Friday / 125   | `X R C` (125)                     | `X R C` (125), no cue; `B` absent                                         |
| Friday / 150   | `B X R C` (150)                   | `B X R C` (150), no cue; keep `B` rather than spend its budget on the cue |
| Friday / 167   | `A B X R C` (167)                 | `B X R C` (166), cue; drops the original instruction `A`                  |
| Friday / 189   | `P A B X R C` (189)               | `A B X R C` (183), cue; drops the common prefix `P`                       |
| Friday / 205   | `P A B X R C` (189)               | `P A B X R C` (205), cue                                                  |
| Findings / 118 | `X R C` (118)                     | `X R C` (118), no cue; `B` absent                                         |
| Findings / 136 | `B X R C` (136)                   | `B X R C` (136), no cue; keep `B`                                         |
| Findings / 153 | `A B X R C` (153)                 | `B X R C` (152), cue; drops `A`                                           |
| Findings / 175 | `P A B X R C` (175)               | `A B X R C` (169), cue; drops `P`                                         |
| Findings / 191 | `P A B X R C` (175)               | `P A B X R C` (191), cue                                                  |

A cue that survives after `A` drops can still say what the answer **didn't** see, but it doesn't restore what the answer **did** see. When `B` itself drops, the next call has neither the provenance distinction nor the correction or finding needed to answer `C`; it should not guess from the surviving reply. Keeping the selected history and omitting the cue when it cannot fit avoids both a dangling reference and eviction to pay for the note.

Keep the flat default for ordinary follow-ups: the cue doesn't change the current task, and at some budgets it displaces history that helps answer it. If a caller needs exact prior-input attribution, the source of that fact is the recorded call selection, not message adjacency or a shared ancestor. A cue for that use would need an unambiguous binding to the visible input and answer and enough spare budget to keep the flat selection. The experiment does not add such a rendering rule, new storage metadata, or a change to display or compaction.
