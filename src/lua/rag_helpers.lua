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

-- Function 2: check_and_compare (COMBINED — replaces check_file_exists + verify_file_hash)
-- Single round-trip: checks existence, compares mtime+size, then hash if needed.
-- Args: local_mtime, local_size, local_content_hash
-- Returns:
--   0 = file does not exist in Redis (new file)
--   1 = file exists AND hash matches AND not dirty (skip)
--   2 = file exists BUT hash/metadata differs OR dirty (needs update)
redis.register_function('check_and_compare', function(keys, args)
    local key = keys[1]
    local local_mtime = args[1]
    local local_size = args[2]
    local local_hash = args[3]

    local exists = redis.call('EXISTS', key)
    if exists == 0 then
        return 0 -- New file
    end

    -- Check context_dirty first (always forces re-upload)
    local dirty = redis.call('HGET', key, 'context_dirty')
    if dirty == 'true' then
        return 2 -- Needs update (context changed)
    end

    -- Fast path: check mtime + size before expensive hash comparison
    local stored_mtime = redis.call('HGET', key, 'file_mtime')
    local stored_size = redis.call('HGET', key, 'file_size')
    if stored_mtime and stored_size and stored_mtime == local_mtime and stored_size == local_size then
        return 1 -- Metadata matches — skip (no need to even check hash)
    end

    -- Slow path: metadata changed, check hash
    local stored_hash = redis.call('HGET', key, 'content_hash')
    if stored_hash and stored_hash == local_hash then
        -- Hash still matches despite metadata change — update stored metadata, skip upload
        redis.call('HSET', key, 'file_mtime', local_mtime, 'file_size', local_size)
        return 1
    end

    return 2 -- Hash mismatch — needs update
end)

-- Function 3: upsert_sync_state (UPDATED — now stores mtime + size)
-- Updates absolute_path, content_hash, mtime, size, and openwebui_file_id without altering context_text.
redis.register_function('upsert_sync_state', function(keys, args)
    local key = keys[1]
    local absolute_path = args[1]
    local content_hash = args[2]
    local openwebui_file_id = args[3]
    local file_mtime = args[4] or ''
    local file_size = args[5] or ''
    redis.call('HSET', key,
        'absolute_path', absolute_path,
        'content_hash', content_hash,
        'openwebui_file_id', openwebui_file_id,
        'context_dirty', 'false',
        'file_mtime', file_mtime,
        'file_size', file_size
    )
    return 1
end)

-- Function 4: get_cleanup_batch
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

-- LEGACY compatibility: keep old functions so existing callers don't crash during rolling updates
redis.register_function('check_file_exists', function(keys, args)
    return redis.call('EXISTS', keys[1])
end)

redis.register_function('verify_file_hash', function(keys, args)
    local key = keys[1]
    local local_hash = args[1]
    local stored = redis.call('HGET', key, 'content_hash')
    if not stored or stored ~= local_hash then
        return 0
    end
    local dirty = redis.call('HGET', key, 'context_dirty')
    if dirty == 'true' then
        return 0
    end
    return 1
end)
