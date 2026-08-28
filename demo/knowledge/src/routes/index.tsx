import { For, Show, createSignal } from 'solid-js'

import { createFileRoute } from '@tanstack/solid-router'

import {
  createRouterClient,
  getRouterClientOptions,
  type RouterClientOptions,
} from '../lib/routerClient'
import { SCENARIO_QUERY_SOURCE_ID, scenarioQueryVector } from '../lib/queryEmbedding'
import type { PreparedGleaphClient } from '../generated'

export const Route = createFileRoute('/')({ component: Home })

/** One executed scenario: the selected columns in display order plus the returned rows. */
type ScenarioResult = {
  columns: string[]
  rows: Record<string, unknown>[]
}

/**
 * Project one typed prepared-query response into the generic table shape. Column order
 * mirrors the operation's RETURN clause.
 */
function project<Row extends object>(
  result: { rows: Row[] },
  columns: string[],
): ScenarioResult {
  return {
    columns,
    rows: result.rows.map((row) => {
      const record: Record<string, unknown> = {}
      for (const column of columns) {
        record[column] = (row as Record<string, unknown>)[column]
      }
      return record
    }),
  }
}

type Scenario = {
  id: string
  label: string
  description: string
  run: (client: PreparedGleaphClient) => Promise<ScenarioResult>
}

const SCENARIOS: Scenario[] = [
  {
    id: 'variable-length-reach',
    label: 'Variable-length reach',
    description:
      'Concepts reachable from "Graph databases" through RELATED_TO in 1..3 hops.',
    run: async (client) =>
      project(await client.variableLengthReach(), ['concept', 'concept_id']),
  },
  {
    id: 'shortest-path',
    label: 'Shortest path',
    description:
      'ANY SHORTEST RELATED_TO path between "Graph databases" and "Vector search".',
    run: async (client) => project(await client.shortestPath(), ['concept_id', 'concept']),
  },
  {
    id: 'team-readable-documents',
    label: 'Semantic search (access-constrained)',
    description:
      'Vector search over document_embedding restricted to public Documents owned by Team ' +
      '"Platform". The $query vector is generated in-page with the same deterministic recipe ' +
      `as the ingested embeddings (seeded by ${SCENARIO_QUERY_SOURCE_ID}), so the top-1 row ` +
      'is fixed to that document.',
    run: async (client) => {
      const query = await scenarioQueryVector()
      return project(await client.teamReadableDocuments({ query }), [
        'document_id',
        'title',
        'similarity',
      ])
    },
  },
  {
    id: 'citation-reach',
    label: 'Citation reach',
    description:
      'Documents reachable from "GraphRAG retrieval" through CITES up to depth 3.',
    run: async (client) =>
      project(await client.citationReach(), ['document_id', 'title', 'cite_edge_id']),
  },
]

function formatCell(value: unknown): string {
  if (value === null || value === undefined) {
    return ''
  }
  if (typeof value === 'bigint') {
    return value.toString()
  }
  if (value instanceof Uint8Array) {
    // Element ids arrive as `Value::Bytes`; render them as hex identities.
    return Array.from(value, (b) => b.toString(16).padStart(2, '0')).join('')
  }
  return String(value)
}

const CONFIG_HINT =
  '.env.local is missing VITE_GLEAPH_ROUTER_CANISTER_ID. Complete the README quickstart ' +
  '(through `pnpm write-env`), then restart this dev server.'

function Home() {
  // Vite bakes the env at build time, so the connection options never change during a session.
  const options: RouterClientOptions | undefined = getRouterClientOptions()
  const clientPromise: Promise<PreparedGleaphClient> | undefined = options
    ? createRouterClient(options)
    : undefined

  const [selectedId, setSelectedId] = createSignal<string>()
  const [result, setResult] = createSignal<ScenarioResult>()
  const [isLoading, setIsLoading] = createSignal(false)
  const [error, setError] = createSignal<unknown>()

  let requestSeq = 0

  const runScenario = async (scenario: Scenario) => {
    const seq = ++requestSeq
    setSelectedId(scenario.id)
    setResult(undefined)
    setError(undefined)
    setIsLoading(true)
    try {
      if (!clientPromise) {
        throw new Error(CONFIG_HINT)
      }
      const outcome = await scenario.run(await clientPromise)
      if (seq !== requestSeq) return
      setResult(outcome)
      setIsLoading(false)
    } catch (err) {
      if (seq !== requestSeq) return
      setError(err)
      setIsLoading(false)
    }
  }

  const active = () => SCENARIOS.find((scenario) => scenario.id === selectedId())

  return (
    <div class="mx-auto max-w-4xl p-8">
      <h1 class="text-3xl font-bold">Knowledge graph scenarios</h1>
      <p class="mt-2 text-gray-600">
        Live results from the registered prepared queries on the local Gleaph network.
      </p>

      <Show
        when={options}
        fallback={
          <div class="mt-4 rounded border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900">
            {CONFIG_HINT}
          </div>
        }
      >
        <div class="mt-6 flex flex-wrap gap-2">
          <For each={SCENARIOS}>
            {(scenario) => (
              <button
                type="button"
                class={`rounded border px-3 py-2 text-left text-sm hover:bg-gray-100 disabled:opacity-50 ${
                  scenario.id === selectedId() ? 'bg-gray-200' : ''
                }`}
                disabled={isLoading()}
                onClick={() => void runScenario(scenario)}
              >
                {scenario.label}
              </button>
            )}
          </For>
        </div>
        <Show when={active()}>
          {(scenario) => <p class="mt-3 text-sm text-gray-600">{scenario().description}</p>}
        </Show>

        <div class="mt-6">
          <Show when={isLoading()}>
            <p class="text-sm text-gray-500">Running…</p>
          </Show>

          <Show
            when={!isLoading() && error() !== undefined}
            fallback={<span />}
          >
            <div class="mt-4 rounded border border-red-300 bg-red-50 p-3 text-sm text-red-900">
              <pre class="overflow-x-auto whitespace-pre-wrap">{stringifyError(error())}</pre>
            </div>
          </Show>

          <Show
            when={!isLoading() && error() === undefined && result() !== undefined}
            fallback={<span />}
          >
            <p class="mb-2 text-sm text-gray-500">
              {result()?.rows.length} row{result()?.rows.length === 1 ? '' : 's'}
            </p>
            <table class="w-full border-collapse text-sm">
              <thead>
                <tr>
                  <For each={result()?.columns}>
                    {(column) => (
                      <th class="border px-2 py-1 text-left font-semibold">{column}</th>
                    )}
                  </For>
                </tr>
              </thead>
              <tbody>
                <For each={result()?.rows}>
                  {(row) => (
                    <tr>
                      <For each={Object.keys(row)}>
                        {(column) => <td class="border px-2 py-1">{formatCell(row[column])}</td>}
                      </For>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </Show>

          <Show when={!selectedId() && !isLoading()}>
            <p class="text-sm text-gray-500">Select a scenario to run it against the Router.</p>
          </Show>
        </div>
      </Show>
    </div>
  )
}

function stringifyError(value: unknown): string {
  if (value instanceof Error) {
    return value.stack ?? `${value.name}: ${value.message}`
  }
  if (typeof value === 'object') {
    try {
      return JSON.stringify(
        value,
        (_key, inner) => (typeof inner === 'bigint' ? inner.toString() : inner),
        2,
      )
    } catch {
      return String(value)
    }
  }
  return String(value)
}
