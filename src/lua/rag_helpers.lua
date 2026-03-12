#!lua name=rag_helpers

-- Function 1: get_formatted_context
-- Fetches context_text and returns it formatted as a blockquote with a configurable header.
redis.register_function('get_formatted_context', function(keys, args)
    local key = keys[1]
    local ctx = redis.call('HGET', key, 'context_text')
    if not ctx or ctx == '' then
        return ''
    end
    local label = redis.call('HGET', 'config:global', 'context_header_label')
    if not label or label == '' then
        label = 'File Context'
    end
    return '> **' .. label .. ':**\n> ' .. ctx .. '\n\n'
end)

-- Function 2: check_file_exists
-- Returns 1 if key exists, 0 if not.
redis.register_function('check_file_exists', function(keys, args)
    return redis.call('EXISTS', keys[1])
end)

-- Function 3: verify_file_hash
-- Compares the stored content_hash with the provided local hash.
-- Returns 1 on match, 0 on mismatch or missing.
redis.register_function('verify_file_hash', function(keys, args)
    local key = keys[1]
    local local_hash = args[1]
    local stored = redis.call('HGET', key, 'content_hash')
    if not stored or stored ~= local_hash then
        return 0
    end
    -- Even if hash matches, force re-upload if context was updated
    local dirty = redis.call('HGET', key, 'context_dirty')
    if dirty == 'true' then
        return 0
    end
    return 1
end)

-- Function 4: upsert_sync_state
-- Updates absolute_path, content_hash, and openwebui_file_id without altering context_text.
redis.register_function('upsert_sync_state', function(keys, args)
    local key = keys[1]
    local absolute_path = args[1]
    local content_hash = args[2]
    local openwebui_file_id = args[3]
    redis.call('HSET', key,
        'absolute_path', absolute_path,
        'content_hash', content_hash,
        'openwebui_file_id', openwebui_file_id,
        'context_dirty', 'false'
    )
    return 1
end)

-- Function 5: get_cleanup_batch
-- Performs a SCAN and retrieves absolute_path and openwebui_file_id for each key.
-- Returns: [new_cursor, key1, path1, id1, key2, path2, id2, ...]
redis.register_function('get_cleanup_batch', function(keys, args)
    local cursor = args[1] or '0'
    local result = redis.call('SCAN', cursor, 'COUNT', 100)
    local new_cursor = result[1]
    local found_keys = result[2]
    local output = { new_cursor }

    for _, k in ipairs(found_keys) do
        -- Skip the global config key
        if k ~= 'config:global' then
            local path = redis.call('HGET', k, 'absolute_path')
            local file_id = redis.call('HGET', k, 'openwebui_file_id')
            if path then
                table.insert(output, k)
                table.insert(output, path)
                table.insert(output, file_id or '')
            end
        end
    end

    return output
end)
