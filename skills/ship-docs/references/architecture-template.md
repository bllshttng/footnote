# Architecture Document Template

---
created: YYYY-MM-DDTHH:MM
status: accepted
---

# [Feature Name]

## Overview

[2-3 sentences: what this is, what it does]

## Component Graph

```mermaid
graph TD
    A[Component A] --> B[Component B]
    A --> C[Component C]
    B --> D[Shared Component]
    C --> D
    style D fill:#e9d5ff,stroke:#7c3aed,stroke-width:2px
```

## Data Flow

```mermaid
flowchart LR
    subgraph Source
        S[Data Source]
    end
    subgraph Transform
        T[Processing]
    end
    subgraph Output
        O[Result]
    end
    S --> T --> O
```

## New Components

| Component | Location | Purpose |
|-----------|----------|---------|

## Modified Components

| Component | Change |
|-----------|--------|

## Dependencies

| Package | Version | Purpose |
|---------|---------|---------|

## Security

- [Security considerations]

## Design Tokens

| Token | Usage |
|-------|-------|

Save to: `internal/web/architecture/{feature}.md`

**Use Mermaid diagrams** for component graphs, data flows, and sequence diagrams. Obsidian renders them natively.
