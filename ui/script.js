async function getInputs() {
  try {
    const resp = await fetch("http://localhost:9997/v3/paths/list");
    const data = await resp.json();
    return {
      data: data.items,
    };
  } catch (err) {
    console.error("Error fetching /inputs", err);
    return null;
  }
}
