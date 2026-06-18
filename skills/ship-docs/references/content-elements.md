# Content Elements

Reusable patterns for how-to guide content.

## Screenshots/UI Descriptions

```
**You'll see:**
┌─────────────────────────────┐
│  Welcome, Sarah!            │
│  [Sign In]  [Sign Out]      │
└─────────────────────────────┘
```

## Common Issues Box

```
**Common issue**: "I don't see my child's name"
**Solution**: Ask the facility to verify your phone number matches their records.
```

## Pro Tips

```
**Pro Tip**: Complete this step while in the parking lot to save time at drop-off.
```

## Reader Testing

**Spawn fresh Claude instances** with no context to test each guide:

```
Read this guide as a [audience member] who has never used the app.
Flag anything confusing, missing, or unclear.
List specific questions you'd have.
```

Common issues reader testing catches:
- Missing navigation instructions ("How do I get to Settings?")
- Assumed knowledge ("What is a compliance score?")
- Confusing button states ("Is gray active or inactive?")
- Missing "who does what" clarity ("Do I enter this or does staff?")

## Writing Principles

- **Show, don't tell**: "Click the green button" not "Select the action"
- **One step at a time**: Break complex flows into atomic steps
- **Anticipate errors**: Include "If you see X, do Y" guidance
- **Use their words**: "Sign in your child" not "Initiate attendance record"
- **Visual markers**: Describe what they'll see at each step
