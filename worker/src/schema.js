import { readFileSync } from 'node:fs'
import Ajv from 'ajv/dist/2020.js'

const dir = new URL('../../worker-contract/', import.meta.url)
const load = (name) => JSON.parse(readFileSync(new URL(name, dir), 'utf8'))

const ajv = new Ajv({ strict: true, strictRequired: false, allErrors: false })
export const validateRequest = ajv.compile(load('worker-request.json'))
export const validateResponse = ajv.compile(load('worker-response.json'))
export const contractDir = dir
