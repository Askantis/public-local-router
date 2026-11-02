#!/usr/bin/env ruby
# Checks the protocol spec for consistency:
#  - every $ref in openapi.yaml resolves, operationIds are unique, every operation has responses
#  - paths, error codes and event types named in PROTOCOL.md match openapi.yaml
#  - every example in examples/ is valid against its schema
#  - the validator rejects known-bad documents (so a broken validator can't pass everything)
# Run: ruby spec/tools/check.rb   (no gems needed)

Encoding.default_external = Encoding::UTF_8
require "json"
require "yaml"

SPEC = File.expand_path("..", __dir__)
$failures = []

def fail!(msg) = $failures << msg

openapi = YAML.safe_load(File.read(File.join(SPEC, "openapi.yaml")), aliases: false)
endpoint_schema = JSON.parse(File.read(File.join(SPEC, "schemas", "endpoint.schema.json")))
protocol = File.read(File.join(SPEC, "PROTOCOL.md"))

# --- A small JSON Schema validator: the subset these schemas use -----------------------------

def resolve(root, ref)
  raise "only local refs are supported: #{ref}" unless ref.start_with?("#/")
  ref.delete_prefix("#/").split("/").reduce(root) do |node, key|
    node.is_a?(Hash) && node.key?(key) ? node[key] : (raise "unresolved $ref #{ref}")
  end
end

# JSON Schema patterns are ECMA-262: ^ and $ anchor the whole string. In Ruby they
# anchor lines, so an anchored pattern must also match the full string here.
def pattern_ok?(pattern, value)
  m = Regexp.new(pattern).match(value)
  return false unless m
  anchored = pattern.start_with?("^") && pattern.end_with?("$")
  !anchored || (m.begin(0).zero? && m.end(0) == value.length)
end

def type_ok?(value, type)
  case type
  when "string" then value.is_a?(String)
  when "integer" then value.is_a?(Integer)
  when "number" then value.is_a?(Numeric)
  when "boolean" then value == true || value == false
  when "object" then value.is_a?(Hash)
  when "array" then value.is_a?(Array)
  when "null" then value.nil?
  else raise "unknown type #{type}"
  end
end

def validate(root, schema, value, path = "$")
  errors = []
  return errors if schema == true || schema.nil?
  if schema.key?("$ref")
    return validate(root, resolve(root, schema["$ref"]), value, path)
  end
  if (t = schema["type"])
    types = Array(t)
    errors << "#{path}: expected #{types.join('|')}, got #{value.class}" unless types.any? { |x| type_ok?(value, x) }
    return errors unless errors.empty?
  end
  errors << "#{path}: #{value.inspect} not in #{schema['enum']}" if schema.key?("enum") && !schema["enum"].include?(value)
  errors << "#{path}: expected const #{schema['const'].inspect}" if schema.key?("const") && schema["const"] != value
  if value.is_a?(String)
    errors << "#{path}: #{value.inspect} doesn't match /#{schema['pattern']}/" if schema["pattern"] && !pattern_ok?(schema["pattern"], value)
    errors << "#{path}: longer than #{schema['maxLength']}" if schema["maxLength"] && value.length > schema["maxLength"]
  end
  if value.is_a?(Numeric)
    errors << "#{path}: below minimum #{schema['minimum']}" if schema["minimum"] && value < schema["minimum"]
    errors << "#{path}: above maximum #{schema['maximum']}" if schema["maximum"] && value > schema["maximum"]
  end
  if value.is_a?(Hash)
    (schema["required"] || []).each { |k| errors << "#{path}: missing required '#{k}'" unless value.key?(k) }
    props = schema["properties"] || {}
    value.each do |k, v|
      if props.key?(k)
        errors.concat(validate(root, props[k], v, "#{path}.#{k}"))
      elsif schema["additionalProperties"] == false
        errors << "#{path}: unexpected property '#{k}'"
      end
    end
  end
  if value.is_a?(Array)
    errors << "#{path}: fewer than #{schema['minItems']} items" if schema["minItems"] && value.size < schema["minItems"]
    errors << "#{path}: items not unique" if schema["uniqueItems"] && value.uniq.size != value.size
    value.each_with_index { |v, i| errors.concat(validate(root, schema["items"], v, "#{path}[#{i}]")) } if schema["items"]
  end
  (schema["allOf"] || []).each { |s| errors.concat(validate(root, s, value, path)) }
  if schema["oneOf"]
    matches = schema["oneOf"].count { |s| validate(root, s, value, path).empty? }
    errors << "#{path}: matches #{matches} of oneOf, expected exactly 1" unless matches == 1
  end
  errors
end

# --- 1. openapi.yaml structure ---------------------------------------------------------------

refs = []
walk = lambda do |node|
  case node
  when Hash then node.each { |k, v| k == "$ref" ? refs << v : walk.(v) }
  when Array then node.each { |v| walk.(v) }
  end
end
walk.(openapi)
refs.uniq.each do |ref|
  resolve(openapi, ref)
rescue => e
  fail!("openapi.yaml: #{e.message}")
end

op_ids = []
openapi["paths"].each do |p, item|
  item.each do |method, op|
    next unless %w[get post put delete patch].include?(method)
    op_ids << op["operationId"]
    fail!("openapi.yaml: #{method.upcase} #{p} has no operationId") unless op["operationId"]
    fail!("openapi.yaml: #{method.upcase} #{p} has no responses") if (op["responses"] || {}).empty?
  end
end
dupes = op_ids.compact.tally.select { |_, n| n > 1 }.keys
fail!("openapi.yaml: duplicate operationIds #{dupes}") unless dupes.empty?

# --- 2. PROTOCOL.md matches openapi.yaml -------------------------------------------------------

norm = ->(p) { p.gsub(/\{[^}]+\}/, "{}").sub(%r{/$}, "") }
spec_paths = openapi["paths"].keys.map(&norm)
mentioned = protocol.scan(%r{`(?:(?:GET|POST|PUT|DELETE) )?(/(?:lr/v1|v1)/[A-Za-z0-9_./{}*-]*)}).flatten.uniq
mentioned.reject { |p| p.include?("*") }.each do |p|
  fail!("PROTOCOL.md mentions #{p}, which openapi.yaml doesn't define") unless spec_paths.include?(norm.(p))
end

protocol.scan(%r{`(?:GET|POST) (/api/[a-z]+)`}).flatten.uniq.each do |p|
  fail!("PROTOCOL.md allows #{p}, which openapi.yaml doesn't define") unless openapi["paths"].key?(p)
end

section = ->(title) { protocol[/^## #{Regexp.escape(title)}\n(.*?)(?=^## )/m, 1] || "" }

doc_codes = section.("12. Errors").scan(/^\| \d{3} \| `([a-z_]+)` \|/).flatten.uniq
api_codes = openapi.dig("components", "schemas", "Error", "properties", "error", "properties", "code", "examples")
fail!("error codes only in PROTOCOL.md: #{doc_codes - api_codes}") unless (doc_codes - api_codes).empty?
fail!("error codes only in openapi.yaml: #{api_codes - doc_codes}") unless (api_codes - doc_codes).empty?
fail!("PROTOCOL.md error table is empty") if doc_codes.empty?

doc_events = section.("10. Events").scan(/^\| `([a-z_.]+)`(?:, `([a-z_.]+)`)?/).flatten.compact.uniq
api_events = openapi.dig("paths", "/lr/v1/events", "get", "responses", "200", "content", "text/event-stream", "x-event-types")
fail!("event types only in PROTOCOL.md: #{doc_events - api_events}") unless (doc_events - api_events).empty?
fail!("event types only in openapi.yaml: #{api_events - doc_events}") unless (api_events - doc_events).empty?

# --- 3. Examples --------------------------------------------------------------------------------

component = ->(name) { { "$ref" => "#/components/schemas/#{name}" } }
examples = {
  "endpoint.json" => [endpoint_schema, endpoint_schema],
  "health.json" => [openapi, component.("Health")],
  "connect-request.json" => [openapi, component.("ConnectRequest")],
  "connect-approved.json" => [openapi, component.("ConnectApproved")],
  "connect-pending.json" => [openapi, component.("ConnectPending")],
  "resident.json" => [openapi, component.("ResidentReport")],
  "models.json" => [openapi, { "type" => "object", "required" => ["models"], "properties" => { "models" => { "type" => "array", "items" => component.("Model") } } }],
  "download-request-mirror.json" => [openapi, component.("DownloadRequest")],
  "download-request-ollama.json" => [openapi, component.("DownloadRequest")],
  "download.json" => [openapi, component.("Download")],
  "transcription-verbose.json" => [openapi, component.("TranscriptionVerbose")],
  "transcription-speakers.json" => [openapi, component.("TranscriptionSpeakers")],
  "diarization.json" => [openapi, component.("Diarization")],
  "error-role-unbound.json" => [openapi, component.("Error")]
}
on_disk = Dir.children(File.join(SPEC, "examples")).select { |f| f.end_with?(".json") }
fail!("examples without a schema mapping: #{on_disk - examples.keys}") unless (on_disk - examples.keys).empty?
examples.each do |file, (root, schema)|
  doc = JSON.parse(File.read(File.join(SPEC, "examples", file)))
  validate(root, schema, doc).each { |e| fail!("examples/#{file}: #{e}") }
rescue Errno::ENOENT
  fail!("examples/#{file} is missing")
end

# The discovery example in PROTOCOL.md must match the one in examples/.
block = protocol[/### 4\.1 The discovery file.*?```json\n(.*?)```/m, 1]
if block
  fail!("PROTOCOL.md discovery example differs from examples/endpoint.json") unless JSON.parse(block) == JSON.parse(File.read(File.join(SPEC, "examples", "endpoint.json")))
else
  fail!("PROTOCOL.md has no discovery example")
end

# --- 4. The validator must reject bad documents ------------------------------------------------

mirror_file = JSON.parse(File.read(File.join(SPEC, "examples", "download-request-mirror.json")))["mirror"]["files"][0]
bad = {
  "endpoint without instance_id" => [endpoint_schema, endpoint_schema, JSON.parse(File.read(File.join(SPEC, "examples", "endpoint.json"))).tap { |d| d.delete("instance_id") }],
  "endpoint on a LAN address" => [endpoint_schema, endpoint_schema, JSON.parse(File.read(File.join(SPEC, "examples", "endpoint.json"))).merge("http" => "http://192.168.1.5:11436")],
  "role with capital letters" => [openapi, component.("RoleDeclaration"), { "role" => "Chapters", "capability" => "chat" }],
  "unknown capability" => [openapi, component.("RoleDeclaration"), { "role" => "chapters", "capability" => "images" }],
  "mirror revision too short" => [openapi, component.("DownloadRequest"), { "model" => { "store" => "hf", "id" => "a/b" }, "source" => "mirror", "mirror" => { "revision" => "abc", "files" => [] } }],
  "speaker id format" => [openapi, component.("Speaker"), { "id" => "speaker_1" }],
  "health with a single protocol number" => [openapi, component.("Health"), { "protocol" => 1, "daemon_version" => "2.0.0", "instance_id" => "x", "capabilities" => [] }],
  "mirror over http" => [openapi, component.("MirrorFile"), mirror_file.merge("url" => "http://example.com/a")],
  "bad etag" => [openapi, component.("MirrorFile"), mirror_file.merge("etag" => "abc")],
  "two oneOf matches impossible" => [openapi, { "oneOf" => [{ "type" => "string" }, { "type" => "string" }] }, "x"]
}
["../evil", "a/../b", "./x", "a/.", "a//b", "/etc/passwd", "a/", "a\\b", "c:d", "x\ny", ""].each do |p|
  bad["mirror path #{p.inspect}"] = [openapi, component.("MirrorFile"), mirror_file.merge("path" => p)]
end
["ggml-model-q8_0.bin", "sub/dir/file.bin", ".gitattributes", "a.b/c..d"].each do |p|
  errs = validate(openapi, component.("MirrorFile"), mirror_file.merge("path" => p))
  fail!("validator rejected a good mirror path #{p.inspect}: #{errs}") unless errs.empty?
end
bad.each do |name, (root, schema, doc)|
  fail!("validator accepted a bad document: #{name}") if validate(root, schema, doc).empty?
end

if $failures.empty?
  puts "spec ok: #{openapi['paths'].size} paths, #{op_ids.size} operations, #{doc_codes.size} error codes, " \
       "#{doc_events.size} event types, #{examples.size} examples, #{bad.size} negative checks"
else
  warn $failures.map { |f| "✗ #{f}" }
  exit 1
end
