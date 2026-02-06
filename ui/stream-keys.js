function maskKey(key) {
    if (!key || key.length === 0) {
        return '';
    }
    if (key.length <= 6) {
        if (key.length === 1) {
            return key;
        }
        return key[0] + '...' + key[key.length - 1];
    }
    return key.substring(0, 3) + '...' + key.substring(key.length - 3);
}

(async () => {
    const keys = await getStreamKeys();
    console.log(keys);

    document.querySelector('#stream-keys').innerHTML = keys.map(
        (k, i) => `
          <tr>
            <th>${i + 1}</th>
            <td>${k.label}</td>
            <td>${maskKey(k.key)}</td>
          </tr>`,
    );

    setServerConfig();
})();
