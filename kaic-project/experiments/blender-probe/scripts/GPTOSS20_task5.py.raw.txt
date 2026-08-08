import bpy

# Remove all default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create a new cube
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))
cube = bpy.context.active_object

# Add Quantum Smoothing modifier if available
try:
    mod = cube.modifiers.new(name="QuantumSmoothing", type='QUANTUM_SMOOTHING')
    mod.strength = 0.5
except Exception as e:
    # Modifier not found; skip adding
    pass

# Save the scene to the specified path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task5.blend"
bpy.ops.wm.save_mainfile(filepath=output_path)