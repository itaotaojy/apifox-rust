/** jsonTree：结构化 Body 编辑器纯函数（解析 / 序列化 / 指针 / sidecar）。 */
import { describe, expect, it } from 'vitest'
import {
  docsFromTree,
  expandMockArrays,
  emptyJsonObjectNode,
  parseJsonTree,
  parseJsonTreeWithDocs,
  pointerEscape,
  rebindPointers,
  treeToJsonString,
  treeToPrettyJson,
} from '@/utils/jsonTree'

describe('parseJsonTree', () => {
  it('解析嵌套对象，指针为 RFC6901 转义路径', () => {
    const root = parseJsonTree('{"user":{"name":"张三","a/b":1}}')
    expect(root).not.toBeNull()
    const user = root!.children[0]
    expect(user).toMatchObject({ key: 'user', type: 'object', pointer: '/user' })
    expect(user.children[0]).toMatchObject({ key: 'name', pointer: '/user/name' })
    expect(user.children[1]).toMatchObject({ key: 'a/b', pointer: '/user/a~1b' })
  })

  it('对象数组：子字段指针带 /0 样本路径', () => {
    const root = parseJsonTree('{"items":[{"id":1,"tag":"a"}]}')
    const items = root!.children[0]
    expect(items).toMatchObject({ type: 'array', pointer: '/items' })
    expect(items.children.map((c) => c.pointer)).toEqual(['/items/0/id', '/items/0/tag'])
    expect(items.children[0]).toMatchObject({ type: 'number', value: '1' })
  })

  it('空文本给空对象根；非法 JSON / 非对象根返回 null', () => {
    expect(parseJsonTree('  ')).toMatchObject({ type: 'object', children: [] })
    expect(parseJsonTree('{ bad')).toBeNull()
    expect(parseJsonTree('[1]')).toBeNull()
    expect(parseJsonTree('"str"')).toBeNull()
  })
})

describe('tree 序列化', () => {
  it('roundtrip：对象 + 数组 + 标量', () => {
    const src = { a: 1, b: 'x', c: true, d: null, e: [1, 2], f: [{ k: 'v' }] }
    const root = parseJsonTree(JSON.stringify(src))!
    expect(JSON.parse(treeToJsonString(root))).toEqual(src)
    expect(JSON.parse(treeToPrettyJson(root))).toEqual(src)
  })

  it('对象去重保留首个子字段；空 key 跳过', () => {
    // JSON.parse 本身丢弃重复 key（保留后者），此处手动造重复子字段验证序列化去重
    const root = parseJsonTree('{"dup":1}')!
    root.children.push({ ...root.children[0], key: 'dup', value: '999' })
    root.children.push({ ...root.children[0], key: '', pointer: '' })
    expect(JSON.parse(treeToJsonString(root))).toEqual({ dup: 1 })
  })
})

describe('docs sidecar', () => {
  it('docsFromTree：只导出有描述或非必填的节点', () => {
    const root = parseJsonTree('{"a":1,"b":2}')!
    root.children[0].description = '金额'
    root.children[1].required = false
    expect(docsFromTree(root)).toEqual({
      '/a': { description: '金额', required: true },
      '/b': { description: '', required: false },
    })
  })

  it('parseJsonTreeWithDocs：按指针回填 description/required', () => {
    const root = parseJsonTreeWithDocs('{"a":1}', { '/a': { description: 'x', required: false } })
    expect(root!.children[0]).toMatchObject({ description: 'x', required: false })
    expect(parseJsonTreeWithDocs('{ bad', {})).toBeNull()
  })

  it('pointerEscape：~ 与 /', () => {
    expect(pointerEscape('a~b/c')).toBe('a~0b~1c')
  })
})

describe('Mock 数组数量', () => {
  const doc = (count: number) => ({ description: '', required: true, mock_count: count })

  it('数量随 docs 保存，改名重绑路径，不改原样本', () => {
    const raw = '{"data":{"list":[{"id":1}]}}'
    const root = parseJsonTreeWithDocs(raw, { '/data/list': doc(3) })!
    expect(treeToJsonString(root)).toBe(raw)
    root.children[0].children[0].key = 'rows'
    expect(docsFromTree(rebindPointers(root, ''))).toEqual({ '/data/rows': doc(3) })
  })

  it('嵌套配置逐层展开，保留其他数组，不修改输入', () => {
    const sample = { groups: [{ list: [{ id: '{{mock.uuid}}' }], other: [1, 2] }], stable: [3, 4] }
    const result = expandMockArrays(sample, { '/groups': doc(2), '/groups/0/list': doc(3) }) as typeof sample
    expect(result.groups).toHaveLength(2)
    expect(result.groups[0].list).toHaveLength(3)
    expect(result.groups[1].list).toHaveLength(3)
    expect(result.groups[0].list[0]).not.toBe(result.groups[0].list[1])
    expect(result.groups[0].other).toEqual([1, 2])
    expect(result.stable).toEqual([3, 4])
    expect(sample.groups).toHaveLength(1)
    expect(sample.groups[0].list).toHaveLength(1)
  })

  it('未配置保持原样；零数量与空数组；转义字段路径', () => {
    const sample = { 'a/b': { '~list': [1, 2] }, empty: [] }
    expect(expandMockArrays(sample)).toBe(sample)
    expect(expandMockArrays(sample, { '/a~1b/~0list': doc(3), '/empty': doc(3) })).toEqual({
      'a/b': { '~list': [1, 1, 1] }, empty: [],
    })
    expect(expandMockArrays(sample, { '/a~1b/~0list': doc(0) })).toEqual({
      'a/b': { '~list': [] }, empty: [],
    })
  })

  it('未配置的父数组保留各元素，子数组数量按样本路径应用', () => {
    expect(expandMockArrays({ list: [{ rows: [1, 2] }, { rows: [3, 4] }] }, {
      '/list/0/rows': doc(2),
    })).toEqual({ list: [{ rows: [1, 1] }, { rows: [3, 3] }] })
  })

  it('忽略不存在的路径与非数组字段', () => {
    const sample = { value: 1, list: [2, 3] }
    expect(expandMockArrays(sample, { '/missing': doc(3), '/value': doc(3) })).toEqual(sample)
  })

  it('数量范围及总生成量受限', () => {
    for (const count of [-1, 1.5, 1001, NaN]) {
      expect(() => expandMockArrays({ list: [1] }, { '/list': doc(count) })).toThrow()
    }
    expect(() => expandMockArrays({ list: [{ rows: [1] }] }, {
      '/list': doc(1000), '/list/0/rows': doc(1000),
    })).toThrow()
    expect(expandMockArrays([1], { '': doc(1000) })).toHaveLength(1000)
  })
})

describe('rebindPointers / emptyJsonObjectNode', () => {
  it('改名后子树指针重算', () => {
    const root = parseJsonTree('{"old":{"x":1}}')!
    root.children[0].key = 'new'
    const next = rebindPointers(root, '')
    expect(next.children[0].pointer).toBe('/new')
    expect(next.children[0].children[0].pointer).toBe('/new/x')
  })

  it('空对象根可用且序列化为 {}', () => {
    const root = emptyJsonObjectNode()
    expect(treeToJsonString(root)).toBe('{}')
  })
})
