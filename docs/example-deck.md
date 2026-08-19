---
title: Example deck
---
<!-- Press F5 to fill the Jim window; Escape restores the floating pane. -->
<!-- layout: title -->

# Example deck

Everything `deck.ft` understands, in four slides.

::: notes
Speaker notes live in a `::: notes` block. They never render on the
slide — they ride along on the `deck.slide` bus topic instead.
:::

---

## Markdown, with emphasis intact

Text keeps its **bold**, *italic* and `inline code`, because slides go
through `md_parse` into rich-text runs that wrap as one block.

- bullets keep their **runs**
- and their `code spans`
- emphasis can even land mid-word: un**bold**ed

***

A `***` rule draws a divider — `---` is taken, it separates slides.

---

## Styled by Glaze

> The whole theme is one `.glz` file.

```rust
let deck = Deck::from_markdown(path);
```

Save `deck.glz` while presenting and the slide restyles itself. Sizes in
the sheet are authored against a 1280x720 reference and scaled to the
pane, so this looks the same in a small pane and full-screen.

---
<!-- dashboard: repo-health -->

## A live dashboard

This slide names a pane group. Advancing here reveals those panes — they
were already running, so nothing respawns and nothing re-fetches.

Wire one up once:

```sh
jimctl group assign --project P --name repo-health \
    --title "Repo Hub" --title "Diff"
```

::: notes
The panes are live. Type in the terminal — it's a real shell.
:::

---
<!-- project: Recursion -->

## A whole project, live

::: notes
Those are the real panes. They keep running while the slide is up.
:::

---
<!-- application: true -->

## The whole application

This is Jim itself: sidebar, canvas, panes, and live input, embedded in
the slide. The recursive deck view is bounded by the view depth limit.
