# Postvim — Concept

Recorded from the exploratory conversation on 2026-10-26. This note consolidates Marceline's stated intentions and the discussion around them. Assistant suggestions are identified separately. **This is a concept note, not a finalized architecture or implementation plan.**

## Premise

Postvim is a working idea for a text editor, preferably written in Rust. Marceline has attempted to build a text editor before; the interest here is primarily philosophical rather than a language rewrite or a new implementation of Vim.

The imagined lineage is **Vim → Neovim → Postvim**. “Post” has the sense it carries in *poststructuralism* and *post-punk*: a response to an earlier movement and a reassessment of its core, rather than simply its next version. The project would recover what makes Vim valuable without inheriting everything that accumulated around it.

Marceline's formulation:

> I imagine a “postvim” as being more committed to a minimal feature set while keeping extensibility open. It should be able to accommodate new features without having to make them a part of itself.

The intended strengths are a minimal nature, modal editing, repackagability, ease of use, and comprehensibility to LLMs. The baggage to avoid includes complex maintenance, ecosystem dependence, and configuration machinery that makes the editor difficult to understand or assemble.

## What the project responds to

These are Marceline's assessments and motivations, not independently researched comparisons of the editors:

- **Neovim:** highly capable, but feels inflated and too willing to become many things—almost Emacs-like in scope.
- **Vim:** closer to the desired purist philosophy, but burdened by programmer cleverness, Vimscript, and its approach to configuration. Marceline characterizes it as very much the project of someone who writes C.
- **Helix:** postponing plugins and changing Vim commands do not make it a stronger successor in the sense intended here. Marceline sees both choices as weakening its claim to be a “postvim.”

The response is therefore neither “Vim rewritten in Rust” nor “a smaller editor that defers extensibility.” Nor is changing the editing language inherently progress. Familiar Vim commands can be something worth preserving rather than something that must be replaced to establish a new identity.

## Minimal core, extensibility from the beginning

Marceline's clarification:

> Perhaps what it doesn't need is first-class support for things like LSPs or treesitter or any of these other coding accoutrements. At least, not as a direct part of the binary. Perhaps they can be packaged in as a first-class plugin we make available, but not necessarily as part of the core system.

The direction is to distinguish **what the editor can support** from **what the core must contain and maintain**.

- Keep the core feature set deliberately bounded.
- Treat extensibility as foundational, not something postponed until the editor is otherwise complete.
- Let capabilities be added without becoming intrinsic parts of the editor.
- Consider official, first-class plugins for coding facilities such as language-server integration and Tree-sitter support, rather than requiring those facilities in the core binary.
- Make the editor easy to package and use without forcing each user to construct a working environment from an uncertain collection of dependencies.

Minimalism should not mean an inadequate extension system, neglected optional features, or a deliberately inconvenient default experience. Equally, official support for a capability need not mean incorporating it into the core.

The exact plugin mechanism, process boundaries, distribution format, and degree of separation from the binary remain undecided.

## Vim continuity without automatic inheritance

The modal system and existing command vocabulary are assets. Reassessment does not require novelty in keybindings or replacing familiar editing habits.

The discussion distinguishes preserving the editing experience from inheriting Vimscript, historical internals, configuration conventions, or every compatibility detail. **The precise compatibility boundary has not been decided.**

An assistant interpretation of the direction:

> Postvim preserves Vim as an editing language, but rethinks the editor as a composable piece of software.

This is a useful formulation to test, not a promise of complete Vim compatibility.

## Repackagability and ease of use

Repackagability is part of the concept, not merely a release-engineering concern. Different uses should be able to receive different capabilities without requiring a different underlying editor or obliging everyone to carry the same coding-oriented machinery.

Assistant suggestions included a basic modal-editor package, a coding-oriented package with official extensions, and a headless editing interface for agents. These are illustrative possibilities, not confirmed deliverables.

A useful design tension is **optional capability versus outsourced completeness**: moving features into plugins should not simply move integration and maintenance work onto users. Official plugins could provide tested compatibility, sensible defaults, and clear ownership while keeping their behavior outside the core's responsibilities.

## Comprehensibility to LLMs

Marceline wants Postvim to be comprehensible to LLMs and viable as an alternative to command-line editing tools. This is an intended use, not a commitment to an embedded AI assistant or a particular model integration.

The assistant suggested exposing explicit, inspectable editing operations rather than requiring an agent to simulate terminal keystrokes and track hidden modal state. Human modal input and programmatic requests could use the same underlying editing semantics.

Possible properties discussed were revision-aware edits, checking expected text before changing it, applying related changes together, and making results reviewable and undoable. **These are design suggestions, not settled requirements or a chosen protocol.**

## Architectural questions to investigate

These questions emerged from the discussion; they remain open:

- What is the smallest coherent editing core, and what must it expose for substantial features to live in plugins?
- Which Vim commands and behaviors should be preserved, and where should compatibility stop?
- How can configuration remain understandable without reproducing Vimscript's burden or requiring an elaborate personal setup?
- Can official language and syntax features use the same public extension interface as third-party features, without privileged hooks or language-specific machinery leaking into the core?
- How should plugins be packaged, versioned, enabled, and supported so that extensibility does not recreate ecosystem dependence?
- What should an LLM-facing editing interface expose, and how should it relate to human modal editing?
- What should Postvim deliberately leave outside its core even when users want that capability?

A caution raised by the assistant: moving code outside the binary is not sufficient by itself to establish a clean boundary. A core full of special cases for official plugins could preserve the same complexity under a different packaging arrangement. The separation must concern responsibilities as well as code location.

## Working synthesis

**A modal text editor that recovers Vim's strengths while keeping its core small and comprehensible, with extensibility present from the outset and substantial optional capabilities available through supported plugins.**

Its minimalism should be expressed in the core's responsibilities and dependencies—not necessarily in the capabilities of the package someone downloads. Rust is the preferred implementation language; the philosophy, compatibility boundary, and extension model are the more important questions to establish first.
