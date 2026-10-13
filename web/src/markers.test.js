import { describe, expect, test } from 'bun:test'
import { wrap } from './markers.js'

describe('wrap', () => {
  test('an empty selection gets the markers with the caret between them', () => {
    expect(wrap('ab', 1, 1, '<u>', '</u>')).toEqual({ value: 'a<u></u>b', caret: 4 })
  })

  test('a selection is wrapped and the caret lands after it', () => {
    expect(wrap('say hi now', 4, 6, '<|im_start|>user\n', '<|im_end|>\n')).toEqual({
      value: 'say <|im_start|>user\nhi<|im_end|>\n now',
      caret: 4 + 17 + 2,
    })
  })

  test('an open-only marker inserts at the caret', () => {
    expect(wrap('', 0, 0, '<|turn>model\n', '')).toEqual({ value: '<|turn>model\n', caret: 13 })
  })
})
