# Agent MCP Configuration

## Context7 Tool Access

All spawned agents should have access to context7 MCP tools for documentation lookup.

### Frontend Agents

Libraries to resolve when needed:
| Library | Context7 ID | Use Case |
|---------|-------------|----------|
| TanStack Router | `/tanstack/router` | Routing patterns, file-based routes |
| TanStack Query | `/tanstack/query` | Data fetching, mutations, caching |
| React | `/facebook/react` | Component patterns, hooks |
| Tailwind CSS | `/tailwindlabs/tailwindcss` | Utility classes, theming |
| Biome | `/biomejs/biome` | Linting, formatting rules |
| Radix UI | `/radix-ui/primitives` | Accessible components |

### Backend Agents

Libraries to resolve when needed:
| Library | Context7 ID | Use Case |
|---------|-------------|----------|
| Supabase | `/supabase/supabase` | Database, auth, RLS policies |
| PostgreSQL | N/A | Use web search for SQL patterns |
| Python | `/python/cpython` | Language features |

### Usage Pattern in Agents

Before implementing with a library, agents should:

1. Resolve library ID:
```typescript
mcp__context7__resolve-library-id({
  libraryName: "tanstack router",
  query: "file-based routing with loaders"
})
```

2. Query for specific patterns:
```typescript
mcp__context7__query-docs({
  libraryId: "/tanstack/router",
  query: "createFileRoute with loader function"
})
```

### Important

- Maximum 3 context7 calls per question
- Prefer specific queries over broad ones
- Cache results mentally for the session
